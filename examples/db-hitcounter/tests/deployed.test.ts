import { existsSync } from "node:fs";
import { writeFile } from "node:fs/promises";
import { delimiter, join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { chromium } from "playwright";
import { afterAll, beforeAll, expect, test } from "vitest";
import { assertUsage, type Usage } from "./billing";
import { Platform } from "./fixture/platform";

let platform: Platform;
const cancel = () => platform?.processes.cancel();
beforeAll(async () => {
  platform = await Platform.create();
  process.on("SIGINT", cancel);
  process.on("SIGTERM", cancel);
  await platform.start();
});
afterAll(async () => {
  try { await platform?.close(); } finally {
    process.off("SIGINT", cancel);
    process.off("SIGTERM", cancel);
  }
});

test("browser writes persist and are priced through the real metering stream", async () => {
  const executablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH
    ?? (process.env.PATH ?? "").split(delimiter)
      .flatMap((directory) => ["chromium", "chromium-browser"].map((name) => join(directory, name))).find(existsSync);
  const browser = await chromium.launch({ headless: true, executablePath });
  const page = await browser.newPage();
  let browserRequests = 0;
  await page.route("**/*", async (route) => {
    const url = new URL(route.request().url());
    if (url.origin === platform.apiUrl && url.pathname.startsWith("/hit/") && route.request().isNavigationRequest()) {
      browserRequests++;
      await route.continue();
    } else await route.abort();
  });
  try {
    for (let index = 0; index < 30; index++) {
      platform.processes.assertAlive();
      const response = await page.goto(`${platform.apiUrl}/hit/${index}`);
      expect(response?.ok()).toBe(true);
      const body = await response!.json();
      expect(body).toMatchObject({ wrote: true, error: null, path: `/hit/${index}` });
      expect(body.readBack).toBeGreaterThan(0);
      expect(body.insertedId).toEqual(expect.any(String));
      expect(await page.locator("body").innerText()).toContain(body.insertedId);
    }
    expect(browserRequests).toBe(30);
  } catch (error) {
    await page.screenshot({ path: join(platform.logs, "browser.png"), fullPage: true });
    throw error;
  } finally { await browser.close(); }

  expect(platform.appId).toMatch(/^[a-zA-Z0-9_-]+$/);
  const rowCount = async () => Number(await platform.sql(`SELECT count(*) FROM "${platform.appId}".hits`));
  const rows = await rowCount();
  expect(rows).toBeGreaterThanOrEqual(browserRequests);
  const requests = platform.readyRequests + browserRequests;
  const readUsage = async (): Promise<Usage> => JSON.parse(await platform.sql(`
    SELECT COALESCE(jsonb_object_agg(metric,total),'{}'::jsonb) FROM (
      SELECT metric, SUM(total) AS total FROM zeroship.usage_aggregates
      WHERE app_id='${platform.appId}' GROUP BY metric
    ) AS usage
  `));
  let usage: Usage = {};
  let previous = "";
  const deadline = Date.now() + 100_000;
  do {
    platform.processes.assertAlive();
    usage = await readUsage();
    const snapshot = JSON.stringify(usage);
    if (usage.requests >= requests && usage.db_writes >= rows && usage.db_rows_written >= rows && snapshot === previous) break;
    previous = snapshot;
    await sleep(1000);
  } while (Date.now() < deadline);
  await writeFile(join(platform.logs, "usage.json"), JSON.stringify({ usage, requests, rows }, null, 2));
  assertUsage(usage, requests, rows);

  const expectedCharge = usage.requests + usage.db_reads + usage.db_writes;
  await expect.poll(async () => {
    platform.processes.assertAlive();
    const response = await fetch(`${platform.controlUrl}/api/apps/${platform.appId}/projected-charge`, {
      headers: { authorization: `Bearer ${platform.bearer}` }, signal: AbortSignal.timeout(5000),
    });
    expect(response.ok).toBe(true);
    return (await response.json()).projected_charge_cents;
  }, { timeout: 45_000, interval: 1000 }).toBe(expectedCharge);

  // The database is an independent oracle: deleting a metered row must fail it.
  await platform.sql(`DELETE FROM "${platform.appId}".hits WHERE ctid IN (SELECT ctid FROM "${platform.appId}".hits LIMIT 1)`);
  const mutatedRows = await rowCount();
  expect(mutatedRows).toBe(rows - 1);
  expect(() => assertUsage(usage, requests, mutatedRows)).toThrow("Write usage must match the physical table");
});
