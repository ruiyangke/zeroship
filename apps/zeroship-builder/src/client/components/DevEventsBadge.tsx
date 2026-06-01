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
//
// Crystal: built over @zeroship/ui — the DS Popover owns open/close,
// click-outside, Escape, anchoring, and the opaque popup surface; a DS
// Badge carries the live count; a DS ScrollArea caps the list height and
// paints the scrollbar chrome. The Cluster / Stack layout primitives
// arrange the header and rows. Only the dev-inspector-specific look lives
// in the co-located DevEventsBadge.css (token-driven, no Tailwind).

import { useEffect, useState } from "react";
import { Badge, Button, Popover, ScrollArea, Stack } from "@zeroship/ui";
import { subscribeEvents, type TrackedEvent } from "../lib/analytics";
import "./DevEventsBadge.css";

export function DevEventsBadge() {
  // Always-call hooks (rules of hooks) — gate render at the bottom.
  const [events, setEvents] = useState<ReadonlyArray<TrackedEvent>>([]);

  useEffect(() => subscribeEvents(setEvents), []);

  if (!import.meta.env.DEV) return null;

  return (
    <div data-testid="dev-events-badge" className="zs-dev-events">
      <Popover>
        <Popover.Trigger
          render={
            <Button
              type="button"
              variant="gray"
              size="small"
              data-testid="dev-events-toggle"
              aria-label={`Dev events: ${events.length}. Click to view.`}
              className="zs-dev-events__toggle"
            >
              <span className="zs-dev-events__toggle-label">ev</span>{" "}
              <Badge
                intent="neutral"
                variant="soft"
                size="sm"
                className="zs-dev-events__count"
              >
                {events.length}
              </Badge>
            </Button>
          }
        />
        <Popover.Portal>
          <Popover.Popup
            side="top"
            align="start"
            data-testid="dev-events-popover"
            aria-label="Recent analytics events"
            className="zs-dev-events__popup"
          >
            <Stack
              direction="row"
              justify="between"
              align="center"
              gap={2}
              className="zs-dev-events__header"
            >
              <span className="zs-dev-events__header-label">recent events</span>
              <span className="zs-dev-events__header-count">
                {events.length} / 50
              </span>
            </Stack>
            {events.length === 0 ? (
              <div className="zs-dev-events__empty">
                No events yet — fire a `track()` call to see it here.
              </div>
            ) : (
              <ScrollArea className="zs-dev-events__scroll">
                <ul className="zs-dev-events__list">
                  {events.map((e, i) => (
                    <li
                      key={`${e.at}-${i}`}
                      data-testid="dev-event-row"
                      className="zs-dev-events__row"
                    >
                      <Stack gap={"half"}>
                        <Stack
                          direction="row"
                          justify="between"
                          align="start"
                          gap={2}
                        >
                          <span className="zs-dev-events__row-name">{e.name}</span>
                          <span className="zs-dev-events__row-time">
                            {relativeTime(e.at)}
                          </span>
                        </Stack>
                        {e.props && (
                          <pre className="zs-dev-events__row-props">
                            {safeStringify(e.props)}
                          </pre>
                        )}
                      </Stack>
                    </li>
                  ))}
                </ul>
              </ScrollArea>
            )}
          </Popover.Popup>
        </Popover.Portal>
      </Popover>
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
