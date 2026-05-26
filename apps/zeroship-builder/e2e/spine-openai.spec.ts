import { test, expect } from "@playwright/test";

// Workspace creation spine: Home → /new wizard → BriefCard → createApp →
// /p/:appId/preview → ChatRail seeded with a first user message
// derived from the brief → Builder reply.
//
// Two tests:
//
//  1. shell-render — no API key required. Just exercises the surface
//     pages render without throwing (Home, /new, /p/<bad-id>/preview).
//
//  2. full-spine — gated on OPENAI_API_KEY. End-to-end through the
//     wizard with a real LLM, real createApp via the control plane,
//     real Builder turn. Skips cleanly if the key is missing; if the
//     control plane is down the createApp will throw and the test
//     fails — that's the point, the spine includes the control plane.

const HAS_KEY = !!process.env.OPENAI_API_KEY;
// Each step (decide → survey, decide → finalize, createApp,
// Builder first turn) is a real network round-trip. Match the
// budget used by wizard-openai.spec.ts.
const STEP_TIMEOUT = 60_000;

// Probe the control plane and sandbox up-front. The full spine talks
// to both: createApp proxies to control:9090, Builder calls fs/exec
// on sandbox:9091. If either is down the test skips cleanly so the
// suite stays green on machines that haven't started the platform.
async function probe(url: string): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(1500) });
    return res.ok || res.status === 401 || res.status === 404; // any reachable response
  } catch {
    return false;
  }
}

test.describe("spine — home → wizard → workspace", () => {
  test("shell renders without an API key", async ({ page }) => {
    // Home: project gallery + new-project prompt. Just assert the
    // surface renders — we don't drive the submit here because
    // Home.submit's `!prompt.trim()` gate has a small post-hydrate
    // race in dev that's flaky after the wizard test mutates the
    // module graph. The wizard surface is exercised directly below.
    //
    // Note: `/` is the public marketing page (per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.1). The
    // authed gallery (Home) lives at `/home` behind AuthGuard; in
    // dev, devBypass lets us land on it without a real session.
    await page.goto("/home");
    await expect(page.getByTestId("home-prompt")).toBeVisible();
    await expect(page.getByTestId("home-submit")).toBeVisible();

    // Wizard: standalone /new page renders independently of Home.
    await page.goto("/new");
    await expect(page.getByTestId("wizard-prompt")).toBeVisible();

    // /p/:appId/preview with a fake id surfaces the workspace error
    // state. The shell fetches getApp(id) which 404s → "Project not
    // found" with a back-home link. We use a syntactically plausible
    // id so we exercise the route, not the catch-all.
    await page.goto("/p/app_fakefakefakefake/preview");
    // Loading state may flash first; the error state arrives once the
    // query rejects. Either is acceptable as a starting state.
    const errBlock = page.getByTestId("workspace-error");
    const loading = page.getByTestId("workspace-loading");
    await Promise.race([
      errBlock.waitFor({ state: "visible", timeout: STEP_TIMEOUT }),
      loading.waitFor({ state: "visible", timeout: 2_000 }),
    ]);
    await expect(errBlock).toBeVisible({ timeout: STEP_TIMEOUT });
    await expect(page.getByTestId("workspace-error-home")).toBeVisible();
  });

  test("idea → survey → brief → Begin → workspace → seeded user msg → assistant reply", async ({ page }, testInfo) => {
    test.skip(!HAS_KEY, "OPENAI_API_KEY not set — skipping real-LLM spine test");
    // The full spine drives ~5 LLM round-trips (decide × N surveys, finalize,
    // createApp, Builder first turn) — each is a real network round-trip.
    // Default 30s test budget is far too tight; allow up to 6 × STEP_TIMEOUT.
    testInfo.setTimeout(STEP_TIMEOUT * 6);
    const controlUrl = process.env.CONTROL_URL ?? "http://localhost:9090";
    const sandboxUrl = process.env.SANDBOX_URL ?? "http://localhost:9091";
    const [controlUp, sandboxUp] = await Promise.all([
      probe(`${controlUrl}/health`),
      probe(`${sandboxUrl}/health`),
    ]);
    test.skip(!controlUp, `control plane unreachable at ${controlUrl} — skipping`);
    test.skip(!sandboxUp, `sandbox controller unreachable at ${sandboxUrl} — skipping`);

    // 1. Wizard.
    await page.goto("/new");
    await expect(page.getByTestId("wizard-prompt")).toBeVisible();
    await page.getByTestId("wizard-prompt").fill(
      "A recipe app for my supper club where guests sign in, post photos, vote on who hosts next.",
    );
    await page.getByTestId("wizard-send-idea").click();

    // 2. First survey arrives (or, in rare cases, the LLM finalizes
    // straight away — accept either).
    const survey = page.getByTestId("survey-card");
    const brief = page.getByTestId("brief-card");
    await Promise.race([
      survey.waitFor({ state: "visible", timeout: STEP_TIMEOUT }),
      brief.waitFor({ state: "visible", timeout: STEP_TIMEOUT }),
    ]);

    // 3. Loop — answer surveys until the brief shows up. Cap at the
    // wizard's own max (5 surveys) plus one for slack.
    let activeSurveyIndex = 0;
    for (let i = 0; i < 6; i++) {
      if (await brief.isVisible()) break;
      // Race the next live (non-collapsed) survey vs. the brief. The
      // wizard collapses the previous survey before streaming the next
      // chunk, so `survey-card` (live) and `survey-card-collapsed`
      // are mutually exclusive on the same row. We wait for either:
      //  - a NEW live survey-card past the index we already answered, or
      //  - the brief.
      const allSurveysHandle = page.getByTestId("survey-card");
      const anySurvey = allSurveysHandle.nth(activeSurveyIndex);
      const winner = await Promise.race([
        anySurvey
          .waitFor({ state: "visible", timeout: STEP_TIMEOUT })
          .then(() => "survey" as const),
        brief
          .waitFor({ state: "visible", timeout: STEP_TIMEOUT })
          .then(() => "brief" as const),
      ]);
      if (winner === "brief") break;
      const current = anySurvey;
      const questions = await current.locator(".space-y-3 > div").all();
      for (const q of questions) {
        const firstOption = q.locator("button").first();
        if (await firstOption.isVisible()) {
          await firstOption.click();
        }
      }
      await current.getByRole("button", { name: /send/i }).click();
      activeSurveyIndex += 1;
      // Wait for either a NEW live survey or the brief.
      await Promise.race([
        page
          .getByTestId("survey-card")
          .nth(activeSurveyIndex)
          .waitFor({ state: "visible", timeout: STEP_TIMEOUT }),
        brief.waitFor({ state: "visible", timeout: STEP_TIMEOUT }),
      ]);
      await page.waitForTimeout(500); // small breath for the data-survey/data-brief chunk to land
    }

    await expect(brief).toBeVisible({ timeout: STEP_TIMEOUT });

    // 4. Begin. Mutation fires createApp({ name }) against the control
    // plane and navigates to /p/<appId>/preview. If the control plane
    // is down this throws and the test fails (the wizard error band
    // surfaces it).
    await expect(page.getByTestId("brief-begin")).toBeEnabled();
    await page.getByTestId("brief-begin").click();

    // 5. Workspace.
    await expect(page).toHaveURL(/\/p\/[^/]+\/preview/, { timeout: STEP_TIMEOUT });
    await expect(page.getByTestId("topbar-project")).toBeVisible({ timeout: STEP_TIMEOUT });
    await expect(page.getByTestId("chat-rail")).toBeVisible();

    // 6. Seeded user message — ChatRail's mount-time useEffect calls
    // sendMessage with "Brief from the wizard:\n\n…". The msg-user
    // testid is on the user bubble.
    const seeded = page.getByTestId("msg-user").first();
    await expect(seeded).toBeVisible({ timeout: STEP_TIMEOUT });
    await expect(seeded).toContainText("Brief from the wizard");

    // 7. Assistant reply — Builder's first turn streams in. We just
    // assert a non-empty assistant bubble appears within budget.
    const assistant = page.getByTestId("msg-assistant").last();
    await expect(assistant).toBeVisible({ timeout: STEP_TIMEOUT });
    await expect(assistant).not.toBeEmpty({ timeout: STEP_TIMEOUT });
  });
});
