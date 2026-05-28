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
import { lsSet } from "../lib/storage";
import { track } from "../lib/analytics";

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

  return (
    <div
      data-testid="product-tour"
      role="dialog"
      aria-modal="true"
      aria-label="Product tour"
      className="fixed inset-0 z-50"
    >
      {/* Backdrop click target — full-viewport, captures the dim
          area's clicks so users can dismiss by clicking outside. The
          spotlight frame sits on top with pointer-events:none so the
          target itself isn't blocked. */}
      <div
        data-testid="product-tour-backdrop"
        onClick={() => complete("skipped")}
        className="absolute inset-0 cursor-pointer"
        style={{ background: "rgba(34,22,12,0.4)" }}
      />

      {/* Spotlight frame around the current target. The box-shadow
          punches a hole in the backdrop by drawing a giant outset
          shadow; we render a thin tomato outline so the eye lands
          on the surface immediately. */}
      {spot && (
        <div
          data-testid="product-tour-spotlight"
          aria-hidden="true"
          className="absolute pointer-events-none"
          style={{
            top: spot.top,
            left: spot.left,
            width: spot.width,
            height: spot.height,
            // Cut a hole in the backdrop with an inverted shadow.
            boxShadow:
              "0 0 0 9999px rgba(34,22,12,0.4), 0 0 0 2px rgba(212,68,46,0.9) inset",
            transition: "all 200ms ease-out",
          }}
        />
      )}

      {/* Tooltip card. Stops click propagation so clicking inside
          doesn't dismiss via the backdrop. */}
      <div
        onClick={(e) => e.stopPropagation()}
        data-testid="product-tour-card"
        className="absolute bg-paper border border-rule shadow-2xl rounded-md p-6 reveal"
        style={{
          width: TOOLTIP_WIDTH,
          top: tooltipPos.top,
          left: tooltipPos.left,
        }}
      >
        <div className="font-sans text-[10px] uppercase tracking-[0.22em] text-tomato mb-2">
          step {step + 1} of {STEPS.length}
        </div>
        <h3 className="font-serif italic font-medium text-[24px] leading-[1.1] mb-2">
          {current.title}
        </h3>
        <p className="font-serif text-[14.5px] text-ink-soft leading-[1.55] mb-5">
          {current.body}
        </p>
        <div className="flex items-center justify-between gap-3">
          <button
            type="button"
            onClick={() => complete("skipped")}
            data-testid="product-tour-skip"
            className="font-serif italic text-[13.5px] text-pencil hover:text-ink bg-transparent border-0 cursor-pointer"
          >
            Skip the tour
          </button>
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={prev}
              disabled={isFirst}
              data-testid="product-tour-prev"
              className="font-serif text-[13px] px-3 py-2 border border-rule bg-paper text-ink cursor-pointer hover:bg-paper-2 disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
            >
              ← Prev
            </button>
            <button
              type="button"
              onClick={next}
              data-testid="product-tour-next"
              className="font-serif text-[13px] px-4 py-2 border border-ink bg-ink text-paper cursor-pointer hover:opacity-90 transition-opacity"
            >
              {isLast ? "Got it" : "Next →"}
            </button>
          </div>
        </div>
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
