// ─── ProductTour — skippable 4-step intro (spec §7.5) ───────────
//
// A tiny coachmark stack. Each step renders a centred card with a
// title + body + Next/Skip buttons. We don't try to position next
// to specific DOM elements (that requires viewport math + portal
// gymnastics that breaks on mobile) — instead we describe each
// surface inline. The user reads, clicks Next, and looks where the
// copy points.
//
// Completion is stored in `localStorage.zeroship_tour_completed` so
// the tour never auto-fires again on the same browser. The TopBar
// "?" pill (or empty-home "Take the tour" CTA) can re-open it any
// time by setting `forceOpen`.

import { useState } from "react";
import { lsSet } from "../lib/storage";
import { track } from "../lib/analytics";

const TOUR_DONE_KEY = "zeroship_tour_completed";

interface Step {
  title: string;
  body: string;
  pointer: string;
}

const STEPS: ReadonlyArray<Step> = [
  {
    title: "The chat",
    body: "On the right — that's where you tell Builder what to make. Type plain English. Builder reads, drafts, and ships.",
    pointer: "Look right →",
  },
  {
    title: "The canvases",
    body: "The pills at the top swap what you see in the main pane: Preview, Files, Logs, Env, Plan, Health, Settings. One project, many lenses.",
    pointer: "Look up ↑",
  },
  {
    title: "The status pill",
    body: "Top-right shows your live URL. Pulse means deployed. Click to open in a new tab.",
    pointer: "Top-right corner",
  },
  {
    title: "Plan & Health",
    body: "Plan is your AI PM — milestones, issues, what's next. Health is your SRE — uptime, perf, incidents. They watch so you don't have to.",
    pointer: "Pills again — try Plan",
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

export function ProductTour({ open, onClose }: ProductTourProps) {
  const [step, setStep] = useState(0);

  if (!open) return null;

  function complete(reason: "finished" | "skipped") {
    lsSet(TOUR_DONE_KEY, "true");
    track("onboarding.tour_" + reason, { last_step: step });
    setStep(0);
    onClose();
  }

  function next() {
    if (step >= STEPS.length - 1) {
      complete("finished");
      return;
    }
    track("onboarding.tour_step", { step: step + 1 });
    setStep(step + 1);
  }

  const current = STEPS[step]!;
  const isLast = step === STEPS.length - 1;

  return (
    <div
      data-testid="product-tour"
      role="dialog"
      aria-modal="true"
      aria-label="Product tour"
      className="fixed inset-0 z-50 flex items-end justify-end p-6 bg-ink/30"
      onClick={() => complete("skipped")}
    >
      <div
        onClick={(e) => e.stopPropagation()}
        className="bg-paper border border-rule shadow-2xl rounded-md max-w-[400px] p-6 reveal"
        data-testid="product-tour-card"
      >
        <div className="font-sans text-[10px] uppercase tracking-[0.22em] text-tomato mb-2">
          step {step + 1} of {STEPS.length}
        </div>
        <h3 className="font-serif italic font-medium text-[24px] leading-[1.1] mb-2">
          {current.title}
        </h3>
        <p className="font-serif text-[14.5px] text-ink-soft leading-[1.55] mb-3">
          {current.body}
        </p>
        <div className="font-sans text-[10.5px] uppercase tracking-[0.18em] text-pencil mb-5">
          {current.pointer}
        </div>
        <div className="flex items-center justify-between">
          <button
            type="button"
            onClick={() => complete("skipped")}
            data-testid="product-tour-skip"
            className="font-serif italic text-[13.5px] text-pencil hover:text-ink bg-transparent border-0 cursor-pointer"
          >
            Skip the tour
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
  );
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
