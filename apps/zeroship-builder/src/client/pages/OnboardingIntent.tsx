// ─── OnboardingIntent — first-run intent picker (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §7.1) ─────
//
// One screen, one question, six chip answers, and a skip link. Runs
// after Signup before the first /home visit. The chosen intent is
// stashed in `localStorage.zeroship_intent` so /home can seed example
// chips and template suggestions later. Skip leaves it unset — the
// home falls back to the static defaults.
//
// Public route on purpose: signup forwards to /onboarding/intent
// even before any session is fully reflected, and we don't want a
// guard race to bounce the user back to /login mid-flow.

import { useNavigate } from "react-router-dom";
import { lsSet } from "../lib/storage";
import { track } from "../lib/analytics";

const INTENT_KEY = "zeroship_intent";

interface Choice {
  id: string;
  label: string;
}

const CHOICES: ReadonlyArray<Choice> = [
  { id: "internal_tool",   label: "an internal tool" },
  { id: "side_project",    label: "a side project" },
  { id: "startup",         label: "a startup" },
  { id: "client_gig",      label: "a client gig" },
  { id: "learning",        label: "something to learn with" },
  { id: "exploring",       label: "just exploring" },
];

export function OnboardingIntent() {
  const navigate = useNavigate();

  function choose(intent: string) {
    lsSet(INTENT_KEY, intent);
    track("onboarding.intent_chosen", { intent });
    navigate("/home", { replace: true });
  }

  function skip() {
    track("onboarding.intent_skipped");
    navigate("/home", { replace: true });
  }

  return (
    <div className="min-h-screen flex items-center justify-center px-5 bg-paper">
      <div
        data-testid="onboarding-intent-page"
        className="w-[640px] max-w-full px-10 py-14 reveal text-center"
      >
        <div className="mb-3 font-sans text-[10px] uppercase tracking-[0.22em] text-tomato">
          a quick question
        </div>
        <h1 className="font-serif font-medium text-[44px] leading-[1.05] -tracking-[0.02em] mb-4">
          What kind of thing are you here to{" "}
          <em className="italic text-tomato">make</em>?
        </h1>
        <p className="font-serif text-[15.5px] text-ink-soft leading-[1.55] max-w-[480px] mx-auto mb-10">
          We'll use this to tailor examples and templates. No wrong answer —
          you can always change your mind later.
        </p>

        <div
          className="flex flex-wrap justify-center gap-2.5 mb-8"
          data-testid="onboarding-intent-choices"
        >
          {CHOICES.map((c) => (
            <button
              key={c.id}
              type="button"
              onClick={() => choose(c.id)}
              data-testid={`onboarding-intent-choice:${c.id}`}
              className="bg-white border border-rule rounded-full px-5 py-2.5 font-serif italic text-[15px] text-ink hover:border-ink hover:bg-paper-2 transition-colors cursor-pointer"
            >
              <span className="text-pencil">"</span>
              {c.label}
              <span className="text-pencil">"</span>
            </button>
          ))}
        </div>

        <button
          type="button"
          onClick={skip}
          data-testid="onboarding-intent-skip"
          className="font-serif italic text-[14px] text-pencil hover:text-ink bg-transparent border-0 cursor-pointer"
        >
          Skip — I'll figure it out →
        </button>
      </div>
    </div>
  );
}

/** Read the stashed intent. Returns null when unset. */
export function readIntent(): string | null {
  try {
    return typeof localStorage !== "undefined"
      ? localStorage.getItem(INTENT_KEY)
      : null;
  } catch {
    return null;
  }
}
