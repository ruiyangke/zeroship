import { existsSync, mkdirSync } from "node:fs";
import { delimiter, join } from "node:path";
import { chromium, type Locator, type Page } from "playwright";
import { expect, inject, test } from "vitest";
import { rpc } from "./rpc";
import { targets } from "./targets";

function browserExecutable(): string | undefined {
  if (process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH) return process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH;
  // NixOS Chromium is linked against the Nix store. Else use Playwright's install.
  return (process.env.PATH ?? "").split(delimiter)
    .flatMap((directory) => ["chromium", "chromium-browser"].map((name) => join(directory, name)))
    .find(existsSync);
}

function panel(page: Page, title: string): Locator {
  return page.locator("form").filter({ has: page.getByRole("heading", { name: title, exact: true }) });
}

test("the dashboard controls reach real KV and recover from RPC failure", async () => {
  const browser = await chromium.launch({ headless: true, executablePath: browserExecutable() });
  try {
    for (const target of targets()) {
      console.info(`KV browser: ${target.name}`);
      await rpc(target.apiUrl, "kv.clear");
      const page = await browser.newPage();
      page.setDefaultTimeout(15_000);
      page.setDefaultNavigationTimeout(30_000);
      const errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));
      try {
        await page.goto(`${target.uiUrl.replace(/\/$/, "")}/`);
        await page.getByRole("heading", { name: "KV Dashboard", exact: true }).waitFor();
        await expect.poll(() => page.getByRole("button", { name: "Refresh", exact: true }).isEnabled()).toBe(true);

        const flag = panel(page, "Feature Flag");
        await flag.getByRole("button", { name: "Enable", exact: true }).click();
        await flag.getByRole("button", { name: "Disable", exact: true }).waitFor();
        expect(await rpc(target.apiUrl, "kv.snapshot")).toMatchObject({ checkoutEnabled: true });

        const text = panel(page, "String + TTL");
        await text.getByLabel("Value", { exact: true }).fill("browser value");
        await text.getByRole("button", { name: "Set", exact: true }).click();
        await expect.poll(() => text.locator("dd").first().textContent()).toBe("browser value");
        await text.getByRole("button", { name: "Persist", exact: true }).click();
        await expect.poll(() => text.locator("dd").allTextContents()).toEqual(["browser value", "true", "none"]);
        expect(await rpc(target.apiUrl, "kv.snapshot")).toMatchObject({ text: { value: "browser value", has: true, ttlMs: null } });
        await text.getByRole("button", { name: "Delete", exact: true }).click();
        await expect.poll(() => text.locator("dd").allTextContents()).toEqual(["-", "false", "none"]);

        const sessions = panel(page, "Sessions");
        await sessions.getByLabel("Name", { exact: true }).fill("Browser session");
        await sessions.getByRole("button", { name: "Create", exact: true }).click();
        const session = sessions.locator("li").filter({ hasText: "Browser session" });
        await session.waitFor();
        await session.getByRole("button", { name: "Delete", exact: true }).click();
        await sessions.getByText("No active sessions", { exact: true }).waitFor();

        const lease = panel(page, "Ephemeral Lease");
        await lease.getByLabel("Owner", { exact: true }).fill("browser-owner");
        await lease.getByRole("button", { name: "Acquire", exact: true }).click();
        await expect.poll(() => lease.locator("dd").first().textContent()).toBe("browser-owner");
        await lease.getByLabel("Owner", { exact: true }).fill("contender");
        await lease.getByRole("button", { name: "Acquire", exact: true }).click();
        await expect.poll(() => lease.locator("dd").last().textContent()).toBe("held");
        expect(await lease.locator("dd").first().textContent()).toBe("browser-owner");
        await lease.getByRole("button", { name: "Reset", exact: true }).click();
        await expect.poll(() => lease.locator("dd").first().textContent()).toBe("-");

        await page.route("**/__zeroship/v1/kv.string.set", (route) => route.fulfill({ status: 503, body: "fixture unavailable" }));
        await text.getByRole("button", { name: "Set", exact: true }).click();
        await page.locator(".notice.error").waitFor();
        expect(await rpc(target.apiUrl, "kv.snapshot")).toMatchObject({ text: { has: false } });
        await page.unroute("**/__zeroship/v1/kv.string.set");
        await text.getByRole("button", { name: "Set", exact: true }).click();
        await expect.poll(() => text.locator("dd").first().textContent()).toBe("browser value");
        expect(errors).toEqual([]);
      } catch (error) {
        const artifacts = inject("dashboardArtifacts") ?? "tests/.artifacts";
        mkdirSync(artifacts, { recursive: true });
        await page.screenshot({ path: join(artifacts, `${target.name.replace(/[^a-z0-9-]/gi, "_")}.png`), fullPage: true });
        throw error;
      } finally {
        await page.close();
        await rpc(target.apiUrl, "kv.clear");
      }
    }
  } finally {
    await browser.close();
  }
});
