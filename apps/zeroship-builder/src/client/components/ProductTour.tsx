// ─── ProductTour — skippable 4-step intro (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §7.5) ───────────
//
// Each step targets a real DOM surface via testid. We measure the
// target's bounding rect, render a 4px outlined frame around it, and
// drop a tooltip card adjacent to it. The rest of the screen is
// dimmed using the box-shadow trick so we don't need a separate
// backdrop element fighting the frame for stacking context.
//
// Keyboard: Esc closes, ←/→ advance. Click on the dimmed area also
// closes (skip).
//
// Completion is stored in `localStorage.zeroship_tour_completed` so
// the tour never auto-fires again on the same browser. The TopBar
// "?" pill (or empty-home "Take the tour" CTA) can re-open it any
// time by setting `forceOpen`.

import { useCallback, useEffect, useLayoutEffect, useState } from "react";
import { Button, Card, Cluster } from "@zeroship/ui";
import { lsSet } from "../lib/storage";
import { track } from "../lib/analytics";
import "./ProductTour.css";

const TOUR_DONE_KEY = "zeroship_tour_completed";

interface Step {
  /** Test-id of the element this step highlights. We re-query each
   *  step change so a step that targets a temporarily-hidden surface
   *  (e.g., a pill that animates in) still works. */
  testid: string;
  /** Where the tooltip floats relative to the target. Auto by default. */
  side?: "top" | "bottom" | "left" | "right";
  title: string;
  body: string;
}

const STEPS: ReadonlyArray<Step> = [
  {
    testid: "chat-composer",
    side: "top",
    title: "The chat",
    body: "Tell Builder what to make. Plain English. It reads, drafts, and ships.",
  },
  {
    testid: "canvas-pills",
    side: "bottom",
    title: "The canvases",
    body: "These pills swap what you see in the main pane. Preview stays first; ops adds logs, env, and settings; code adds files.",
  },
  {
    testid: "topbar-url",
    side: "bottom",
    title: "The status pill",
    body: "Top-right shows your live URL. Pulse means deployed. Click to open in a new tab.",
  },
  {
    testid: "canvas-pills",
    side: "bottom",
    title: "Control the depth",
    body: "Start in Maker when you just want the result. Add ops for runtime evidence, then add code when you need the files.",
  },
];

export interface ProductTourProps {
  /** Render the tour. Parent owns visibility so the trigger can be
   *  anywhere (TopBar "?", empty-home CTA, etc.). */
  open: boolean;
  /** Called when the user finishes or skips. Parent should flip
   *  `open` to false. */
  onClose: () => void;
}

interface Rect {
  top: number;
  left: number;
  width: number;
  height: number;
}

const PADDING = 8;
const TOOLTIP_GAP = 12;
const TOOLTIP_WIDTH = 360;

export function ProductTour({ open, onClose }: ProductTourProps) {
  const [step, setStep] = useState(0);
  const [rect, setRect] = useState<Rect | null>(null);

  const complete = useCallback(
    (reason: "finished" | "skipped") => {
      lsSet(TOUR_DONE_KEY, "true");
      track("onboarding.tour_" + reason, { last_step: step });
      setStep(0);
      onClose();
    },
    [onClose, step],
  );

  const next = useCallback(() => {
    if (step >= STEPS.length - 1) {
      complete("finished");
      return;
    }
    track("onboarding.tour_step", { step: step + 1 });
    setStep(step + 1);
  }, [complete, step]);

  const prev = useCallback(() => {
    setStep((s) => Math.max(0, s - 1));
  }, []);

  // Measure the target on mount, on step change, and on resize/scroll.
  // We don't need rAF — the read is cheap and runs at most once per
  // event burst because React batches state updates.
  useLayoutEffect(() => {
    if (!open) return;
    const target = STEPS[step];
    if (!target) return;

    function measure() {
      const el = document.querySelector<HTMLElement>(
        `[data-testid="${target!.testid}"]`,
      );
      if (!el) {
        setRect(null);
        return;
      }
      // Scroll into view first — if the surface is below the fold,
      // the spotlight has nothing to land on. `block:"nearest"` keeps
      // the page steady when the target is already visible.
      el.scrollIntoView({ behavior: "smooth", block: "nearest" });
      const r = el.getBoundingClientRect();
      setRect({
        top: r.top,
        left: r.left,
        width: r.width,
        height: r.height,
      });
    }
    measure();
    const obs = new ResizeObserver(measure);
    obs.observe(document.body);
    window.addEventListener("scroll", measure, true);
    window.addEventListener("resize", measure);
    return () => {
      obs.disconnect();
      window.removeEventListener("scroll", measure, true);
      window.removeEventListener("resize", measure);
    };
  }, [open, step]);

  // Esc closes; ←/→ navigate. Listener attaches only while open so
  // a closed tour doesn't intercept page-wide arrow-key shortcuts.
  useEffect(() => {
    if (!open) return;
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") {
        e.preventDefault();
        complete("skipped");
      } else if (e.key === "ArrowRight") {
        e.preventDefault();
        next();
      } else if (e.key === "ArrowLeft") {
        e.preventDefault();
        prev();
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, complete, next, prev]);

  if (!open) return null;
  const current = STEPS[step]!;
  const isLast = step === STEPS.length - 1;
  const isFirst = step === 0;

  // Spotlight rect (target + padding) — clamped to viewport so a
  // partially-offscreen target still produces a sensible frame.
  const spot = rect
    ? {
        top: Math.max(4, rect.top - PADDING),
        left: Math.max(4, rect.left - PADDING),
        width: Math.min(window.innerWidth - 8, rect.width + PADDING * 2),
        height: Math.min(window.innerHeight - 8, rect.height + PADDING * 2),
      }
    : null;

  // Tooltip placement. Side preference comes from the step; we fall
  // back to whichever side has more room if the preferred side would
  // run off-screen.
  const tooltipPos = computeTooltipPosition(current.side, spot);

  // Runtime layout values flow as inline custom properties consumed by
  // ProductTour.css. These are measured positions, not design constants
  // — the CSS owns the token-driven chrome; the component owns the
  // geometry.
  const spotVars = spot
    ? ({
        "--tour-spot-top": `${spot.top}px`,
        "--tour-spot-left": `${spot.left}px`,
        "--tour-spot-width": `${spot.width}px`,
        "--tour-spot-height": `${spot.height}px`,
      } as React.CSSProperties)
    : undefined;
  const cardVars = {
    "--tour-card-width": `${TOOLTIP_WIDTH}px`,
    "--tour-card-top": `${tooltipPos.top}px`,
    "--tour-card-left": `${tooltipPos.left}px`,
  } as React.CSSProperties;

  return (
    <div
      data-testid="product-tour"
      role="dialog"
      aria-modal="true"
      aria-label="Product tour"
      className="zs-tour"
    >
      {/* Backdrop click target — full-viewport, captures the dim
          area's clicks so users can dismiss by clicking outside. The
          spotlight frame sits on top with pointer-events:none so the
          target itself isn't blocked. */}
      <div
        data-testid="product-tour-backdrop"
        onClick={() => complete("skipped")}
        className="zs-tour__backdrop"
      />

      {/* Spotlight frame around the current target. The box-shadow
          punches a hole in the dim by drawing a giant outset shadow; we
          render a thin accent ring so the eye lands on the surface
          immediately. */}
      {spot && (
        <div
          data-testid="product-tour-spotlight"
          aria-hidden="true"
          className="zs-tour__spotlight"
          style={spotVars}
        />
      )}

      {/* Tooltip card. Stops click propagation so clicking inside
          doesn't dismiss via the backdrop. */}
      <div
        onClick={(e) => e.stopPropagation()}
        data-testid="product-tour-card"
        className="zs-tour__card"
        style={cardVars}
      >
        <Card variant="elevated">
          <Card.Header>
            <span className="zs-tour__eyebrow" data-testid="product-tour-step">
              step {step + 1} of {STEPS.length}
            </span>
            <Card.Title>{current.title}</Card.Title>
            <Card.Description>{current.body}</Card.Description>
          </Card.Header>
          <Card.Footer align="between">
            <Button
              variant="plain"
              size="small"
              onClick={() => complete("skipped")}
              data-testid="product-tour-skip"
            >
              Skip the tour
            </Button>
            <Cluster gap={2}>
              <Button
                variant="gray"
                size="small"
                onClick={prev}
                disabled={isFirst}
                data-testid="product-tour-prev"
              >
                ← Prev
              </Button>
              <Button
                variant="filled"
                size="small"
                onClick={next}
                data-testid="product-tour-next"
              >
                {isLast ? "Got it" : "Next →"}
              </Button>
            </Cluster>
          </Card.Footer>
        </Card>
      </div>
    </div>
  );
}

/**
 * Place the tooltip relative to the spotlight rect. We try the
 * preferred side first; if the tooltip would clip the viewport, we
 * fall back to whichever side has more space.
 *
 * When `spot` is null (target not on the page), we centre the
 * tooltip — better than leaving it pinned to a stale rect.
 */
function computeTooltipPosition(
  preferred: Step["side"] = "bottom",
  spot: { top: number; left: number; width: number; height: number } | null,
): { top: number; left: number } {
  if (!spot) {
    // Centred fallback — viewport math is fine in the browser.
    const vw = typeof window === "undefined" ? 1024 : window.innerWidth;
    const vh = typeof window === "undefined" ? 768 : window.innerHeight;
    return {
      top: Math.max(20, vh / 2 - 100),
      left: Math.max(20, vw / 2 - TOOLTIP_WIDTH / 2),
    };
  }
  const vw = window.innerWidth;
  const vh = window.innerHeight;

  // Estimated tooltip height — we don't measure it, since the card
  // size is roughly fixed. 220px is generous for our copy length.
  const TOOLTIP_HEIGHT = 220;

  function tryPlace(side: "top" | "bottom" | "left" | "right") {
    let top = 0;
    let left = 0;
    if (side === "bottom") {
      top = spot!.top + spot!.height + TOOLTIP_GAP;
      left = spot!.left + spot!.width / 2 - TOOLTIP_WIDTH / 2;
    } else if (side === "top") {
      top = spot!.top - TOOLTIP_GAP - TOOLTIP_HEIGHT;
      left = spot!.left + spot!.width / 2 - TOOLTIP_WIDTH / 2;
    } else if (side === "right") {
      top = spot!.top + spot!.height / 2 - TOOLTIP_HEIGHT / 2;
      left = spot!.left + spot!.width + TOOLTIP_GAP;
    } else {
      top = spot!.top + spot!.height / 2 - TOOLTIP_HEIGHT / 2;
      left = spot!.left - TOOLTIP_GAP - TOOLTIP_WIDTH;
    }
    // Clamp into the viewport (with an 8px gutter).
    left = Math.max(8, Math.min(vw - TOOLTIP_WIDTH - 8, left));
    top = Math.max(8, Math.min(vh - TOOLTIP_HEIGHT - 8, top));
    // Compute how much of the tooltip would lay over the spot — we
    // want the chosen side to NOT overlap the spotlight. Score by
    // overlap area; lower is better.
    const overlapW = Math.max(
      0,
      Math.min(left + TOOLTIP_WIDTH, spot!.left + spot!.width) -
        Math.max(left, spot!.left),
    );
    const overlapH = Math.max(
      0,
      Math.min(top + TOOLTIP_HEIGHT, spot!.top + spot!.height) -
        Math.max(top, spot!.top),
    );
    return { top, left, score: overlapW * overlapH };
  }

  const candidates: ("top" | "bottom" | "left" | "right")[] = [
    preferred,
    preferred === "bottom" ? "top" : "bottom",
    "right",
    "left",
  ];
  let best = tryPlace(candidates[0]!);
  for (let i = 1; i < candidates.length; i += 1) {
    const c = tryPlace(candidates[i]!);
    if (c.score < best.score) best = c;
  }
  return { top: best.top, left: best.left };
}

/** True when the user finished/skipped the tour at least once. */
export function tourCompleted(): boolean {
  try {
    return typeof localStorage !== "undefined"
      && localStorage.getItem(TOUR_DONE_KEY) === "true";
  } catch {
    return false;
  }
}
