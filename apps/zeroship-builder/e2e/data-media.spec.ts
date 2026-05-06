import { test, expect, type Page } from "@playwright/test";

// Data and Media canvas smoke test.
//
// Strategy mirrors plan-health.spec.ts: probe the control plane,
// create a fresh app if it's reachable, otherwise skip cleanly so
// the suite stays green on machines without the platform running.
//
// Both canvases are backed by in-memory stubs in server/agents.ts —
// they don't need OPENAI_API_KEY and don't talk to the control plane
// at runtime. The probe is only here to establish a real appId so
// WorkspaceShell mounts the canvas branches (the appId-less catch-all
// path renders a "No project selected" placeholder instead).

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
  const name = `e2e-data-${Math.random().toString(36).slice(2, 8)}`;
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

test.describe("Data + Media canvases", () => {
  let app: CreatedApp | null = null;

  test.beforeAll(async () => {
    const up = await probe(`${CONTROL_URL}/health`);
    if (!up) return;
    try {
      app = await createApp();
    } catch (e) {
      console.warn(`[data-media] createApp threw, will skip: ${(e as Error).message}`);
    }
  });

  test.afterAll(async () => {
    if (app) await deleteApp(app.id);
  });

  test.beforeEach(async () => {
    test.skip(!app, `control plane unreachable at ${CONTROL_URL} or createApp failed — skipping data/media tests`);
  });

  test("data canvas renders five subtab pills", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
    await selectPill(page, "data");
    await expect(page.getByTestId("data-canvas")).toBeVisible();
    // All five subtab pills present.
    await expect(page.getByTestId("data-subtab:tables")).toBeVisible();
    await expect(page.getByTestId("data-subtab:schema")).toBeVisible();
    await expect(page.getByTestId("data-subtab:indexes")).toBeVisible();
    await expect(page.getByTestId("data-subtab:migrations")).toBeVisible();
    await expect(page.getByTestId("data-subtab:backups")).toBeVisible();
    // Tables is the default — its content region shows up first.
    await expect(page.getByTestId("data-subtab-content:tables")).toBeVisible();
  });

  test("each subtab swaps the content region", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    await selectPill(page, "data");
    await expect(page.getByTestId("data-canvas")).toBeVisible();

    // Schema sub-tab — placeholder copy.
    await page.getByTestId("data-subtab:schema").click();
    await expect(page.getByTestId("data-subtab-content:schema")).toBeVisible();
    await expect(page.getByTestId("data-schema-empty")).toBeVisible();

    // Indexes sub-tab — sample rows.
    await page.getByTestId("data-subtab:indexes").click();
    await expect(page.getByTestId("data-subtab-content:indexes")).toBeVisible();
    await expect(page.getByTestId("data-index-row:users_pkey")).toBeVisible();

    // Migrations sub-tab — sample rows.
    await page.getByTestId("data-subtab:migrations").click();
    await expect(page.getByTestId("data-subtab-content:migrations")).toBeVisible();
    await expect(
      page.getByTestId("data-migration-row:20260428.090000"),
    ).toBeVisible();

    // Backups sub-tab — trigger button + at least one snapshot row.
    await page.getByTestId("data-subtab:backups").click();
    await expect(page.getByTestId("data-subtab-content:backups")).toBeVisible();
    await expect(page.getByTestId("data-backup-trigger")).toBeVisible();
  });

  test("clicking a table row opens the row browser", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    await selectPill(page, "data");
    await expect(page.getByTestId("data-canvas")).toBeVisible();
    await expect(page.getByTestId("data-subtab-content:tables")).toBeVisible();

    // Sample tables include `users` — open it.
    await page.getByTestId("data-table-row:users").click();
    await expect(page.getByTestId("data-row-browser")).toBeVisible();
    // Back button returns to the tables list.
    await page.getByTestId("data-row-browser-back").click();
    await expect(page.getByTestId("data-subtab-content:tables")).toBeVisible();
  });

  test("media canvas renders drop zone + tile grid", async ({ page }) => {
    await page.goto(`/p/${app!.id}/preview`);
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
    await selectPill(page, "media");
    await expect(page.getByTestId("media-canvas")).toBeVisible();
    // Drop zone is the load-bearing element.
    await expect(page.getByTestId("media-dropzone")).toBeVisible();
    // Seeded tiles are present (3 sample entries — see seedMedia()).
    await expect(page.getByTestId("media-grid")).toBeVisible();
    await expect(page.getByTestId("media-tile:seed_logo")).toBeVisible();
    await expect(page.getByTestId("media-tile:seed_brief")).toBeVisible();
  });
});
