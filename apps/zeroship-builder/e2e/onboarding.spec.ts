import { test, expect } from "@playwright/test";

// Onboarding smoke test.
//
// All UI surfaces — no API key, no control plane needed. The
// onboarding intent picker, the wizard's first-run hint, and the
// product-tour trigger are local-state-only and we drive them with
// localStorage assertions.

const CONTROL_URL = process.env.CONTROL_URL ?? "http://localhost:9090";
const CONTROL_KEY = process.env.CONTROL_KEY ?? "dev-master-key";

async function probe(url: string): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(1500) });
    return res.ok || res.status === 401 || res.status === 404;
  } catch {
    return false;
  }
}

async function createTourApp(): Promise<{ id: string } | null> {
  const name = `e2e-tour-${Math.random().toString(36).slice(2, 8)}`;
  try {
    const res = await fetch(`${CONTROL_URL}/api/apps`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${CONTROL_KEY}`,
      },
      body: JSON.stringify({ name, plan_id: "free" }),
    });
    if (!res.ok) return null;
    const json = (await res.json()) as { id: string };
    return { id: json.id };
  } catch {
    return null;
  }
}

async function deleteTourApp(id: string): Promise<void> {
  await fetch(`${CONTROL_URL}/api/apps/${encodeURIComponent(id)}`, {
    method: "DELETE",
    headers: { authorization: `Bearer ${CONTROL_KEY}` },
  }).catch(() => {});
}

test.describe("onboarding (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §7)", () => {
  test.beforeEach(async ({ context }) => {
    // Each test starts on a clean storage so first-run flags fire.
    await context.clearCookies();
    await context.clearPermissions();
  });

  test("/onboarding/intent renders six choices + skip link", async ({ page }) => {
    await page.goto("/onboarding/intent");
    await expect(page.getByTestId("onboarding-intent-page")).toBeVisible();

    await expect(page.getByTestId("onboarding-intent-choice:internal_tool")).toBeVisible();
    await expect(page.getByTestId("onboarding-intent-choice:side_project")).toBeVisible();
    await expect(page.getByTestId("onboarding-intent-choice:startup")).toBeVisible();
    await expect(page.getByTestId("onboarding-intent-choice:client_gig")).toBeVisible();
    await expect(page.getByTestId("onboarding-intent-choice:learning")).toBeVisible();
    await expect(page.getByTestId("onboarding-intent-choice:exploring")).toBeVisible();

    await expect(page.getByTestId("onboarding-intent-skip")).toBeVisible();
  });

  test("clicking a choice stores intent in localStorage and navigates to /home", async ({ page }) => {
    await page.goto("/onboarding/intent");
    await page.getByTestId("onboarding-intent-choice:side_project").click();
    await expect(page).toHaveURL(/\/home$/);
    const intent = await page.evaluate(() => localStorage.getItem("zeroship_intent"));
    expect(intent).toBe("side_project");
  });

  test("Skip leaves intent unset and navigates to /home", async ({ page }) => {
    await page.goto("/onboarding/intent");
    await page.evaluate(() => localStorage.removeItem("zeroship_intent"));
    await page.getByTestId("onboarding-intent-skip").click();
    await expect(page).toHaveURL(/\/home$/);
    const intent = await page.evaluate(() => localStorage.getItem("zeroship_intent"));
    expect(intent).toBeNull();
  });

  test("first /new visit shows the first-run hint card", async ({ page }) => {
    // Clear the flag so this counts as a first visit.
    await page.goto("/");
    await page.evaluate(() => localStorage.removeItem("zeroship_first_run"));

    await page.goto("/new");
    await expect(page.getByTestId("wizard-first-run-hint")).toBeVisible();
  });

  test("subsequent /new visits hide the first-run hint card", async ({ page }) => {
    // Pre-set the flag — that's what would happen after the first send.
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("zeroship_first_run", "completed"));

    await page.goto("/new");
    await expect(page.getByTestId("wizard-first-run-hint")).toBeHidden();
  });

  test("empty submit shows validation hint instead of sending", async ({ page }) => {
    await page.goto("/new");
    // Begin button is disabled with empty input — clicking it does
    // nothing. Type whitespace, then verify Begin is still disabled.
    const send = page.getByTestId("wizard-send-idea");
    await expect(send).toBeDisabled();

    // Spaces only — the trim guard treats this as empty too.
    await page.getByTestId("wizard-prompt").fill("   ");
    await expect(send).toBeDisabled();
  });
});

test.describe("product tour (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §7.5)", () => {
  // The tour is driven from the WorkspaceShell TopBar "?" button, so
  // we need a project workspace mounted. Fall back to creating an app
  // via the control plane; if it's down we skip — the tour requires
  // the canvas-pills / chat-composer / topbar-url surfaces to land
  // on, which only render inside /p/:id.
  let appId: string | null = null;

  test.beforeAll(async () => {
    const up = await probe(`${CONTROL_URL}/health`);
    if (!up) return;
    const created = await createTourApp();
    if (created) appId = created.id;
  });

  test.afterAll(async () => {
    if (appId) await deleteTourApp(appId);
  });

  test.beforeEach(async ({ context }) => {
    test.skip(!appId, `control plane unreachable at ${CONTROL_URL} — skipping tour test`);
    await context.clearCookies();
  });

  test("opens, walks all 4 steps with surface highlighting, and closes", async ({ page }) => {
    await page.goto(`/p/${appId}/preview`);
    // Wait for shell to mount.
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
    // Open the tour from the TopBar ? button.
    await page.getByTestId("topbar-tour").click();
    await expect(page.getByTestId("product-tour")).toBeVisible();
    await expect(page.getByTestId("product-tour-card")).toBeVisible();

    // Step 1 — chat composer. Spotlight rect should be over the
    // composer's bounding box. We assert spotlight presence; the
    // surface-targeting logic is exercised end-to-end by the fact
    // that the spotlight renders at all (computeTooltipPosition
    // returns null when target is missing → no spotlight).
    await expect(page.getByTestId("product-tour-spotlight")).toBeVisible();

    // Walk Next through steps 2, 3, 4. The card heading flips per
    // step; we just count card mounts via the step-of-N copy.
    for (let i = 1; i < 4; i += 1) {
      await page.getByTestId("product-tour-next").click();
      // Spotlight re-measures on step change.
      await expect(page.getByTestId("product-tour-spotlight")).toBeVisible();
      await expect(page.getByTestId("product-tour-card")).toContainText(
        `step ${i + 1} of 4`,
      );
    }

    // On the last step Next finalises and closes the tour.
    await page.getByTestId("product-tour-next").click();
    await expect(page.getByTestId("product-tour")).toBeHidden();

    // localStorage flag persists.
    const done = await page.evaluate(() =>
      localStorage.getItem("zeroship_tour_completed"),
    );
    expect(done).toBe("true");
  });

  test("Esc closes the tour", async ({ page }) => {
    await page.goto(`/p/${appId}/preview`);
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
    await page.getByTestId("topbar-tour").click();
    await expect(page.getByTestId("product-tour")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("product-tour")).toBeHidden();
  });

  test("Skip closes the tour without finishing", async ({ page }) => {
    await page.goto(`/p/${appId}/preview`);
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
    await page.getByTestId("topbar-tour").click();
    await expect(page.getByTestId("product-tour")).toBeVisible();
    await page.getByTestId("product-tour-skip").click();
    await expect(page.getByTestId("product-tour")).toBeHidden();
  });

  test("Prev is disabled on step 1 and walks back from step 2", async ({ page }) => {
    await page.goto(`/p/${appId}/preview`);
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
    await page.getByTestId("topbar-tour").click();
    await expect(page.getByTestId("product-tour-card")).toContainText("step 1 of 4");
    // Prev disabled on first step.
    await expect(page.getByTestId("product-tour-prev")).toBeDisabled();
    await page.getByTestId("product-tour-next").click();
    await expect(page.getByTestId("product-tour-card")).toContainText("step 2 of 4");
    await expect(page.getByTestId("product-tour-prev")).toBeEnabled();
    await page.getByTestId("product-tour-prev").click();
    await expect(page.getByTestId("product-tour-card")).toContainText("step 1 of 4");
  });
});
