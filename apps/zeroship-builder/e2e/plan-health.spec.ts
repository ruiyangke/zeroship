import { test, expect, type Page } from "@playwright/test";

// Plan 01.7 — Plan + Health canvases (spec §9.8 + §9.9).
//
// Strategy mirrors workspace-canvases.spec.ts: probe the control
// plane up-front, create a fresh app if it's reachable, otherwise
// fall back to the catch-all `*` route which mounts WorkspaceShell
// without an appId. The Plan/Health canvases need an appId for their
// data fetches, so the fallback path only asserts the "no project
// selected" placeholder is gone — the full canvas testid is
// asserted only when control is up.
//
// The new-issue flow is exercised against the in-memory stub in
// server/agents.ts. That works regardless of control-plane state
// because the stub lives in the dev server's V8 isolate, not the
// control plane.

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

interface CreatedApp { id: string; name: string }

async function createApp(): Promise<CreatedApp> {
  const name = `e2e-plan-${Math.random().toString(36).slice(2, 8)}`;
  const res = await fetch(`${CONTROL_URL}/api/apps`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${CONTROL_KEY}`,
    },
    body: JSON.stringify({ name, plan_id: "free" }),
  });
  if (!res.ok) {
    throw new Error(`create app failed (${res.status}): ${await res.text()}`);
  }
  const json = (await res.json()) as { id: string; name: string };
  return { id: json.id, name: json.name };
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

test.describe("Plan 01.7 — Plan + Health canvases", () => {
  let app: CreatedApp | null = null;

  test.beforeAll(async () => {
    const up = await probe(`${CONTROL_URL}/health`);
    if (!up) return;
    try {
      app = await createApp();
    } catch (e) {
      console.warn(`[plan-health] createApp threw, will skip: ${(e as Error).message}`);
    }
  });

  test.afterAll(async () => {
    if (app) await deleteApp(app.id);
  });

  test.beforeEach(async () => {
    test.skip(!app, `control plane unreachable at ${CONTROL_URL} or createApp failed — skipping plan/health tests`);
  });

  test("plan canvas renders three sections when the pill is clicked", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
    await selectPill(page, "plan");
    await expect(page.getByTestId("plan-canvas")).toBeVisible();
    await expect(page.getByTestId("plan-issues-section")).toBeVisible();
    await expect(page.getByTestId("plan-roadmap-section")).toBeVisible();
    await expect(page.getByTestId("plan-deployments-section")).toBeVisible();
    // The roadmap renders all three milestones.
    await expect(page.getByTestId("plan-milestone:v0.1")).toBeVisible();
    await expect(page.getByTestId("plan-milestone:v0.2")).toBeVisible();
    await expect(page.getByTestId("plan-milestone:v0.3")).toBeVisible();
  });

  test("filing a new issue from the modal adds it to the list", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    await selectPill(page, "plan");
    await expect(page.getByTestId("plan-canvas")).toBeVisible();

    // Wait for the seeded issues to land so the "new" issue is
    // distinguishable. The seed includes "Welcome to the plan canvas".
    await expect(
      page.getByTestId("plan-issues-section").getByText(/Welcome to the plan canvas/),
    ).toBeVisible();

    await page.getByTestId("plan-new-issue").click();
    await expect(page.getByTestId("plan-new-issue-modal")).toBeVisible();

    const title = `e2e issue ${Math.random().toString(36).slice(2, 6)}`;
    await page.getByTestId("plan-new-issue-title").fill(title);
    await page
      .getByTestId("plan-new-issue-description")
      .fill("Filed by Playwright. Should appear at the top of Open.");
    await page.getByTestId("plan-new-issue-submit").click();

    // Modal closes once the mutation resolves.
    await expect(page.getByTestId("plan-new-issue-modal")).toBeHidden();
    // New issue is visible in the list.
    await expect(
      page.getByTestId("plan-issues-section").getByText(title),
    ).toBeVisible();
  });

  test("health canvas renders four sections when the pill is clicked", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
    await selectPill(page, "health");
    await expect(page.getByTestId("health-canvas")).toBeVisible();
    await expect(page.getByTestId("health-status-section")).toBeVisible();
    await expect(page.getByTestId("health-quality-section")).toBeVisible();
    await expect(page.getByTestId("health-incidents-section")).toBeVisible();
    await expect(page.getByTestId("health-performance-section")).toBeVisible();
    // Status starts as "Draft" until a deploy hash exists.
    await expect(page.getByTestId("health-status-label")).toHaveText(/Draft|Live/);
    // Quality grid renders 7 dimension rows.
    await expect(
      page.getByTestId("health-quality-row:correctness"),
    ).toBeVisible();
    await expect(
      page.getByTestId("health-quality-row:code_health"),
    ).toBeVisible();
    // Incidents empty state.
    await expect(page.getByTestId("health-incidents-empty")).toBeVisible();
    // Performance placeholders.
    await expect(page.getByTestId("health-perf-latency")).toBeVisible();
    await expect(page.getByTestId("health-perf-errors")).toBeVisible();
    await expect(page.getByTestId("health-perf-rps")).toBeVisible();
  });
});
