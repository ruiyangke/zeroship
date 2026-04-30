// ─── Preview iframe — does it actually show the deployed app? ────
//
// PreviewTab renders an iframe pointed at /apps/<name>/. In dev that
// path goes through Vite, which proxies it to the gateway. This test
// deploys a known-marker app and verifies the iframe DOM actually
// contains the deployed content — not Vite's SPA fallback or a 404.

import { test, expect } from "@playwright/test";
import {
  createApp,
  deleteApp,
  rpc,
  uniqueAppName,
  visit,
  type AppRecord,
} from "./helpers";
import { buildZsapp } from "./zsapp-helper";

test.describe("preview iframe", () => {
  test.setTimeout(60_000);
  test.use({ actionTimeout: 15_000, navigationTimeout: 15_000 });

  let app: AppRecord;
  const PREVIEW_HTML =
    `<!doctype html><html><head><title>e2e-preview</title></head>` +
    `<body><h1 data-testid="deployed-marker">e2e preview ok</h1></body></html>`;
  const SERVER_JS =
    `export default { fetch() { return new Response(${JSON.stringify(PREVIEW_HTML)}, ` +
    `{ headers: { "content-type": "text/html; charset=utf-8" } }); } };`;

  test.beforeAll(async ({ request }) => {
    app = await createApp(request, uniqueAppName("e2e-prev"));
    // Deploy a `.zsapp` archive (tar.zst with manifest + blobs/<hash>).
    // The control plane stopped accepting raw JS in the artifact-layout
    // redesign — see `docs/reference/zsapp.md`.
    const archive = buildZsapp(SERVER_JS);
    const r = await request.post(
      `http://localhost:9090/api/apps/${app.id}/deploy`,
      {
        headers: {
          authorization: "Bearer dev-master-key",
          "content-type": "application/x-zsapp",
        },
        data: archive,
      },
    );
    if (!r.ok()) throw new Error(`deploy failed: ${r.status()} ${await r.text()}`);

    // The gateway syncs routes from the control plane every ~3s. Poll
    // until /apps/<name>/ is reachable end-to-end before any test runs.
    const deadline = Date.now() + 15_000;
    while (Date.now() < deadline) {
      const probe = await request.get(`http://localhost:8001/apps/${app.name}/`);
      if (probe.ok()) return;
      await new Promise((r) => setTimeout(r, 500));
    }
    throw new Error("gateway never picked up the deployed route");
  });

  test.afterAll(async ({ request }) => {
    if (app) await deleteApp(request, app.id);
  });

  test("workspace iframe loads the deployed app body", async ({ page }) => {
    await visit(page, `/p/${app.id}/preview`);
    await expect(page.getByTestId("preview-tab")).toBeVisible();

    // ProjectWorkspace pulls the app record once on mount; deploy_hash
    // must be set for the iframe to mount. Wait for the live URL pill.
    await expect(page.getByTestId("topbar-url")).toContainText(
      `${app.name}.zeroship.app`,
    );

    // The iframe element with src pointing at /apps/<name>/...
    const iframe = page.locator('iframe[title*="Preview"]');
    await expect(iframe).toBeVisible();
    await expect(iframe).toHaveAttribute(
      "src",
      new RegExp(`^/apps/${app.name}/`),
    );

    // Read the iframe's content via Playwright's FrameLocator. This
    // proves the proxied gateway response reached the browser, not
    // Vite's SPA shell.
    const frame = page.frameLocator('iframe[title*="Preview"]');
    await expect(frame.getByTestId("deployed-marker")).toHaveText(
      /e2e preview ok/i,
    );
  });

  test("the proxied URL returns the deployed body directly", async ({ request }) => {
    const r = await request.get(`http://localhost:5173/apps/${app.name}/`);
    expect(r.ok()).toBe(true);
    const body = await r.text();
    expect(body).toContain("e2e preview ok");
    expect(body.toLowerCase()).not.toContain("<!doctype html>\n<html lang=\"en\">"); // not vite shell
  });

  test("undeployed app shows the placeholder, not a stale iframe", async ({ page, request }) => {
    const fresh = await createApp(request, uniqueAppName("e2e-prev-undep"));
    try {
      await visit(page, `/p/${fresh.id}/preview`);
      await expect(page.getByTestId("preview-tab")).toBeVisible();
      await expect(page.getByText(/Nothing's been built yet/i)).toBeVisible();
      // No iframe should mount when there's no deploy_hash.
      await expect(page.locator('iframe[title*="Preview"]')).toHaveCount(0);
    } finally {
      await deleteApp(request, fresh.id);
    }
  });
});
