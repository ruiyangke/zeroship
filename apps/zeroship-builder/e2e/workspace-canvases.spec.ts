import { test, expect, type Page } from "@playwright/test";

// Plan 01.6 — workspace canvases (files, logs, env, settings).
//
// Strategy: probe the control plane up-front. If it's reachable we
// create a fresh app and exercise each canvas against real data. If
// it's down, the WorkspaceShell renders its "Project not found"
// state — these tests skip cleanly so the suite stays green on
// machines without the platform running.
//
// The canvas testids are emitted by FilesCanvas / LogsCanvas /
// EnvCanvas / SettingsCanvas. Files-canvas needs the sandbox
// controller for live data, but renders its own editorial empty
// state when the controller is down — we still see the canvas
// testid in either case.

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
  const name = `e2e-canvas-${Math.random().toString(36).slice(2, 8)}`;
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

test.describe("Plan 01.6 — workspace canvases", () => {
  let app: CreatedApp | null = null;

  test.beforeAll(async () => {
    const up = await probe(`${CONTROL_URL}/health`);
    if (!up) return;
    try {
      app = await createApp();
    } catch (e) {
      console.warn(`[canvases] createApp threw, will skip: ${(e as Error).message}`);
    }
  });

  test.afterAll(async () => {
    if (app) await deleteApp(app.id);
  });

  test.beforeEach(async () => {
    test.skip(!app, `control plane unreachable at ${CONTROL_URL} or createApp failed — skipping canvas tests`);
  });

  test("files canvas renders when the pill is clicked", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    // Wait for the workspace shell to land (preview canvas appears).
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
    await selectPill(page, "files");
    await expect(page.getByTestId("files-canvas")).toBeVisible();
  });

  test("logs canvas renders when the pill is clicked", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
    await selectPill(page, "logs");
    await expect(page.getByTestId("logs-canvas")).toBeVisible();
    // Filter pills should be visible too.
    await expect(page.getByTestId("logs-search")).toBeVisible();
    await expect(page.getByTestId("logs-autoscroll")).toBeVisible();
  });

  test("env canvas renders when the pill is clicked", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
    await selectPill(page, "env");
    await expect(page.getByTestId("env-canvas")).toBeVisible();
    // Both sections should be present.
    await expect(page.getByTestId("env-variables-section")).toBeVisible();
    await expect(page.getByTestId("env-secrets-section")).toBeVisible();
  });

  test("settings canvas renders when the pill is clicked", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
    await selectPill(page, "settings");
    await expect(page.getByTestId("settings-canvas")).toBeVisible();
    // Plans block + delete button are the load-bearing UI.
    await expect(page.getByTestId("settings-plans")).toBeVisible();
    await expect(page.getByTestId("settings-delete")).toBeVisible();
  });
});
