// ─── OnboardingIntent — first-run intent picker (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §7.1) ─────
//
// One screen, one question, six choice tiles, and a skip link. Runs
// after Signup before the first /home visit. The chosen intent is
// stashed in `localStorage.zeroship_intent` so /home can seed example
// chips and template suggestions later. Skip leaves it unset — the
// home falls back to the static defaults.
//
// Public route on purpose: signup forwards to /onboarding/intent
// even before any session is fully reflected, and we don't want a
// guard race to bounce the user back to /login mid-flow.
//
// Crystal: a Center-anchored column (Stack) of headline + subtitle,
// a Grid of interactive Card choice tiles, and a plain Button skip
// link. Single step, so no Stepper. The eyebrow / headline emphasis /
// quote-glyph styling lives in the co-located stylesheet over --zs-*
// tokens; the public exports + data hooks are unchanged.

import { useNavigate } from "react-router-dom";
import { Button, Card, Center, Grid, Stack } from "@zeroship/ui";
import { lsSet } from "../lib/storage";
import { track } from "../lib/analytics";
import "./OnboardingIntent.css";

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
    <Center minHeight="100dvh" className="zb-onboarding-intent">
      <Stack
        gap={8}
        align="center"
        data-testid="onboarding-intent-page"
        className="zb-onboarding-intent__panel"
      >
        <Stack gap={4} align="center" className="zb-onboarding-intent__intro">
          <span className="zb-onboarding-intent__eyebrow">a quick question</span>
          <h1 className="zb-onboarding-intent__title">
            What kind of thing are you here to{" "}
            <em className="zb-onboarding-intent__emphasis">make</em>?
          </h1>
          <p className="zb-onboarding-intent__lede">
            We'll use this to tailor examples and templates. No wrong answer —
            you can always change your mind later.
          </p>
        </Stack>

        <Grid
          minColWidth="13rem"
          gap={3}
          data-testid="onboarding-intent-choices"
          className="zb-onboarding-intent__choices"
        >
          {CHOICES.map((c) => (
            <Card
              key={c.id}
              variant="outline"
              interactive
              onClick={() => choose(c.id)}
              data-testid={`onboarding-intent-choice:${c.id}`}
              className="zb-onboarding-intent__choice"
            >
              <Card.Content className="zb-onboarding-intent__choice-body">
                <span className="zb-onboarding-intent__quote" aria-hidden="true">
                  "
                </span>
                {c.label}
                <span className="zb-onboarding-intent__quote" aria-hidden="true">
                  "
                </span>
              </Card.Content>
            </Card>
          ))}
        </Grid>

        <Button
          variant="plain"
          onClick={skip}
          data-testid="onboarding-intent-skip"
          className="zb-onboarding-intent__skip"
        >
          Skip — I'll figure it out →
        </Button>
      </Stack>
    </Center>
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
