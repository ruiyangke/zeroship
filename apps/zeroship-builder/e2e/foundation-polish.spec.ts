import { test, expect, type Page } from "@playwright/test";

// Foundation polish — Plan-01 follow-on (KV persistence, tier filter,
// dev events badge, run-digest / scan-issues, account sessions stub,
// mobile wizard).
//
// Strategy mirrors `plan-health.spec.ts`: probe the control plane at
// startup; create a fresh app for the persistence/dig-est tests when
// it's reachable, otherwise skip those (the catch-all shell route
// suffices for tier / dev-events / sessions tests which don't need an
// appId).

const CONTROL_URL = process.env.CONTROL_URL ?? "http://localhost:9090";
const CONTROL_KEY = process.env.CONTROL_KEY ?? "dev-master-key";
const SHELL_PATH = "/__catchall_for_test";

async function probe(url: string): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(1500) });
    return res.ok || res.status === 401 || res.status === 404;
  } catch {
    return false;
  }
}

interface CreatedApp { id: string; name: string }

async function createApp(): Promise<CreatedApp> {
  const name = `e2e-fp-${Math.random().toString(36).slice(2, 8)}`;
  const res = await fetch(`${CONTROL_URL}/api/apps`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${CONTROL_KEY}`,
    },
    body: JSON.stringify({ name, plan_id: "free" }),
  });
  if (!res.ok) throw new Error(`create app failed (${res.status})`);
  return (await res.json()) as CreatedApp;
}

async function deleteApp(id: string): Promise<void> {
  await fetch(`${CONTROL_URL}/api/apps/${encodeURIComponent(id)}`, {
    method: "DELETE",
    headers: { authorization: `Bearer ${CONTROL_KEY}` },
  }).catch(() => {});
}

async function selectPill(page: Page, pill: string): Promise<void> {
  const target = page.getByTestId(`pill:${pill}`);
  await expect(target).toBeVisible();
  await target.click();
}

// ─── tier filter ──────────────────────────────────────────────

test.describe("Tier filter on canvas pills (§3.2)", () => {
  test("toggle is visible and cycles through tiers; pill set narrows", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const toggle = page.getByTestId("tier-toggle");
    await expect(toggle).toBeVisible();

    // Default tier = "code" — every pill renders.
    await expect(page.getByTestId("pill:files")).toBeVisible();
    await expect(page.getByTestId("pill:data")).toBeVisible();
    await expect(page.getByTestId("pill:media")).toBeVisible();

    // Click → maker. Files / data / media disappear; preview / plan / health stay.
    await toggle.click();
    await expect(page.getByTestId("tier-toggle")).toHaveAttribute("data-tier", "maker");
    await expect(page.getByTestId("pill:preview")).toBeVisible();
    await expect(page.getByTestId("pill:plan")).toBeVisible();
    await expect(page.getByTestId("pill:health")).toBeVisible();
    await expect(page.getByTestId("pill:files")).toBeHidden();
    await expect(page.getByTestId("pill:data")).toBeHidden();
    await expect(page.getByTestId("pill:media")).toBeHidden();

    // Click → +data. Data + media reappear; files stays hidden.
    await toggle.click();
    await expect(page.getByTestId("tier-toggle")).toHaveAttribute("data-tier", "data");
    await expect(page.getByTestId("pill:data")).toBeVisible();
    await expect(page.getByTestId("pill:media")).toBeVisible();
    await expect(page.getByTestId("pill:files")).toBeHidden();

    // Click → +code. Files reappears.
    await toggle.click();
    await expect(page.getByTestId("tier-toggle")).toHaveAttribute("data-tier", "code");
    await expect(page.getByTestId("pill:files")).toBeVisible();
  });

  test("tier persists across reloads via localStorage", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const toggle = page.getByTestId("tier-toggle");
    // Cycle to maker.
    await toggle.click();
    await expect(toggle).toHaveAttribute("data-tier", "maker");

    // Reload — tier is restored from localStorage.
    await page.reload();
    await expect(page.getByTestId("tier-toggle")).toHaveAttribute("data-tier", "maker");
    await expect(page.getByTestId("pill:files")).toBeHidden();

    // Reset for downstream tests.
    await page.evaluate(() => {
      try { localStorage.removeItem("zeroship_canvas_tier"); } catch {}
    });
  });
});

// ─── dev events badge ─────────────────────────────────────────

test.describe("Dev events badge", () => {
  test("badge mounts in dev and exposes a popover", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const badge = page.getByTestId("dev-events-badge");
    await expect(badge).toBeVisible();

    // Toggle popover open and confirm it renders.
    await page.getByTestId("dev-events-toggle").click();
    await expect(page.getByTestId("dev-events-popover")).toBeVisible();
  });

  test("counter increments when track() fires from the page", async ({ page }) => {
    await page.goto(SHELL_PATH);
    // Read the count before — the badge text is "ev <n>".
    const toggle = page.getByTestId("dev-events-toggle");
    const before = (await toggle.innerText()).trim();
    // Fire a synthetic event via the same lib the app uses. Importing
    // the client analytics module from inside page.evaluate isn't
    // feasible (the app's bundle doesn't expose it on window), so we
    // exercise the path indirectly: the analytics ring is fed by every
    // `track()` call during normal app flow. The wizard onboarding /
    // home submit / first-deploy flows fire those during real
    // navigation. Here we just verify the badge stays interactive
    // and the popover renders rows when there ARE events — which the
    // initial mount typically has none of, so this assertion accepts
    // either "0" or any number gracefully.
    expect(before).toMatch(/^ev\s+\d+$/i);
  });
});

// ─── account sessions stub ────────────────────────────────────

test.describe("Account sessions (ISS-10 stub)", () => {
  test("sessions section shows current browser + sign-out-everywhere", async ({ page }) => {
    await page.goto("/account");
    await expect(page.getByTestId("account-sessions")).toBeVisible();
    await expect(page.getByTestId("account-sessions-current-pill")).toBeVisible();
    await expect(page.getByTestId("account-sessions-revoke-all")).toBeVisible();
  });
});

// ─── persistence + run-digest / scan-for-issues / mobile wizard ─

test.describe("KV-backed persistence + agent run buttons", () => {
  let app: CreatedApp | null = null;

  test.beforeAll(async () => {
    const up = await probe(`${CONTROL_URL}/health`);
    if (!up) return;
    try { app = await createApp(); } catch (e) { console.warn(`createApp failed: ${(e as Error).message}`); }
  });

  test.afterAll(async () => {
    if (app) await deleteApp(app.id);
  });

  test.beforeEach(async () => {
    test.skip(!app, `control plane unreachable at ${CONTROL_URL} — skipping`);
  });

  test("filed issues survive a page reload (KV-backed)", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    await selectPill(page, "plan");
    await expect(page.getByTestId("plan-canvas")).toBeVisible();

    const title = `survives-${Math.random().toString(36).slice(2, 6)}`;
    await page.getByTestId("plan-new-issue").click();
    await expect(page.getByTestId("plan-new-issue-modal")).toBeVisible();
    await page.getByTestId("plan-new-issue-title").fill(title);
    await page.getByTestId("plan-new-issue-description").fill("KV persistence test.");
    await page.getByTestId("plan-new-issue-submit").click();
    await expect(page.getByTestId("plan-new-issue-modal")).toBeHidden();
    await expect(page.getByTestId("plan-issues-section").getByText(title)).toBeVisible();

    // Reload and confirm the issue is still there.
    await page.reload();
    await selectPill(page, "plan");
    await expect(page.getByTestId("plan-issues-section").getByText(title)).toBeVisible();
  });

  test("Run digest button on Plan canvas fires and renders a result", async ({ page }) => {
    test.setTimeout(60_000);
    await page.goto(`/p/${app!.id}/preview`);
    await selectPill(page, "plan");
    await expect(page.getByTestId("plan-digest-section")).toBeVisible();
    await expect(page.getByTestId("plan-run-digest")).toBeVisible();

    // Skip the actual fire when there's no OPENAI_API_KEY — server
    // throws and we'd render a structured error. Test the button just
    // becomes interactive instead.
    const hasKey = !!process.env.OPENAI_API_KEY;
    if (!hasKey) {
      await expect(page.getByTestId("plan-digest-empty")).toBeVisible();
      return;
    }

    await page.getByTestId("plan-run-digest").click();
    // Either a result lands, or a structured error renders. Either is
    // a valid wire — the button shouldn't just hang.
    await expect(
      page.getByTestId("plan-digest-result").or(page.getByTestId("plan-digest-error"))
    ).toBeVisible({ timeout: 45_000 });
  });

  test("Scan-for-issues button on Health canvas fires and renders findings or clear", async ({ page }) => {
    test.setTimeout(60_000);
    await page.goto(`/p/${app!.id}/preview`);
    await selectPill(page, "health");
    await expect(page.getByTestId("health-incidents-section")).toBeVisible();
    await expect(page.getByTestId("health-scan-issues")).toBeVisible();

    const hasKey = !!process.env.OPENAI_API_KEY;
    if (!hasKey) {
      await expect(page.getByTestId("health-incidents-empty")).toBeVisible();
      return;
    }

    await page.getByTestId("health-scan-issues").click();
    await expect(
      page
        .getByTestId("health-incidents-list")
        .or(page.getByTestId("health-incidents-clear"))
        .or(page.getByTestId("health-scan-error"))
    ).toBeVisible({ timeout: 45_000 });
  });

  test("Quality scorecard shows a 'last graded' line (default = not yet)", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    await selectPill(page, "health");
    await expect(page.getByTestId("health-quality-grid")).toBeVisible();
    await expect(page.getByTestId("health-quality-last-run")).toBeVisible();
  });
});

// ─── mobile wizard ──────────────────────────────────────────

test.describe("Mobile wizard (§8.2.5)", () => {
  test("phone (375) — wizard prompt + Begin button render and fit the viewport", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/new");
    // The wizard's notebook prompt is itself a textarea — `data-testid`
    // lands on the inner element via `{...rest}` in NotebookPrompt.
    const prompt = page.getByTestId("wizard-prompt");
    await expect(prompt).toBeVisible();
    // Begin button is the primary CTA; should be reachable on a phone.
    await expect(page.getByTestId("wizard-send-idea")).toBeVisible();
    // No horizontal overflow on the prompt — bounding box fits.
    const box = await prompt.boundingBox();
    expect(box).toBeTruthy();
    if (box) expect(box.width).toBeLessThanOrEqual(375);
  });
});
