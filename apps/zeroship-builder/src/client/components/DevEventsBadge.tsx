// ─── DevEventsBadge — floating telemetry inspector (dev only) ──
//
// A small floating chip in the bottom-LEFT corner that shows the
// running count of fired analytics events. Click to open a popover
// listing the most recent ones (name + props + relative time). Helps
// developers verify that `track()` calls are firing without console-
// diving.
//
// Hidden in production builds — the chip mounts only when
// `import.meta.env.DEV` is true. The badge is non-interactive for
// non-dev users; never bothers them.
//
// Positioning note: the badge sits bottom-left so it doesn't overlap
// the chat composer's Send / Stop buttons (which sit bottom-right
// inside the chat sidebar). Sharing the bottom-right corner with
// real interactive surfaces caused click-intercept failures (e.g.,
// e2e/chat-openai.spec.ts "stop button cancels an in-flight stream").

import { useEffect, useRef, useState } from "react";
import { subscribeEvents, type TrackedEvent } from "../lib/analytics";

export function DevEventsBadge() {
  // Always-call hooks (rules of hooks) — gate render at the bottom.
  const [events, setEvents] = useState<ReadonlyArray<TrackedEvent>>([]);
  const [open, setOpen] = useState(false);
  const popoverRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => subscribeEvents(setEvents), []);

  useEffect(() => {
    if (!open) return;
    function onDown(e: MouseEvent) {
      const node = popoverRef.current;
      if (node && !node.contains(e.target as Node)) setOpen(false);
    }
    window.addEventListener("mousedown", onDown);
    return () => window.removeEventListener("mousedown", onDown);
  }, [open]);

  if (!import.meta.env.DEV) return null;

  return (
    <div
      data-testid="dev-events-badge"
      className="fixed bottom-3 left-3 z-50 font-mono text-[11px]"
    >
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        data-testid="dev-events-toggle"
        aria-label={`Dev events: ${events.length}. Click to view.`}
        className="bg-ink/85 text-paper border border-ink rounded-full px-2.5 py-1 shadow-sm hover:bg-ink cursor-pointer focus:outline-2 focus:outline-tomato focus:outline-offset-2"
      >
        ev <span className="ml-1 text-paper/70">{events.length}</span>
      </button>
      {open && (
        <div
          ref={popoverRef}
          data-testid="dev-events-popover"
          className="absolute bottom-9 left-0 w-[320px] max-h-[60vh] overflow-y-auto bg-paper border border-rule shadow-xl"
        >
          <div className="px-3 py-2 border-b border-rule flex items-center justify-between">
            <span className="font-sans text-[10px] uppercase tracking-[0.18em] text-ink-soft">
              recent events
            </span>
            <span className="font-mono text-[10px] text-pencil">{events.length} / 50</span>
          </div>
          {events.length === 0 ? (
            <div className="px-3 py-4 font-serif italic text-[13px] text-pencil">
              No events yet — fire a `track()` call to see it here.
            </div>
          ) : (
            <ul className="m-0 p-0 list-none">
              {events.map((e, i) => (
                <li
                  key={`${e.at}-${i}`}
                  data-testid="dev-event-row"
                  className="px-3 py-2 border-b border-rule-2"
                >
                  <div className="flex items-baseline justify-between gap-2">
                    <span className="font-mono text-[11px] text-ink truncate">{e.name}</span>
                    <span className="font-serif italic text-[10.5px] text-pencil whitespace-nowrap">
                      {relativeTime(e.at)}
                    </span>
                  </div>
                  {e.props && (
                    <pre className="font-mono text-[10.5px] text-ink-soft whitespace-pre-wrap break-words m-0 mt-0.5">
                      {safeStringify(e.props)}
                    </pre>
                  )}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}

function safeStringify(v: unknown): string {
  try {
    return JSON.stringify(v, null, 0);
  } catch {
    return "[unserialisable]";
  }
}

function relativeTime(iso: string): string {
  const t = new Date(iso).getTime();
  if (!Number.isFinite(t)) return "—";
  const diff = Date.now() - t;
  if (diff < 1_000) return "just now";
  if (diff < 60_000) return `${Math.floor(diff / 1_000)}s ago`;
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  return `${Math.floor(diff / 3_600_000)}h ago`;
}
