import { expect as playwrightExpect } from "@playwright/test";
import { launchChromium, pageFixtures, type PageWatch } from "@zeroship/example-testkit";
import type { Browser, Page } from "playwright";
import { afterAll, beforeAll, describe, inject, test as base } from "vitest";
import { target, targets, type Target } from "./targets";

const expect = playwrightExpect.configure({ timeout: 15_000 });

/** The todos src/server.ts holds, in its order. */
const SERVER_TODOS = ["Read the .zship spec", "Build the CSR demo", "Verify the manifest", "Stretch \u2014 wire up SSR"];
/** The server todos whose text contains "the". */
const THE_MATCHES = ["Read the .zship spec", "Build the CSR demo", "Verify the manifest"];

/** A Vitest `test` whose page is a fresh, watched page over `where`. */
function browserTest(where: Target) {
  let browser: Browser;
  beforeAll(async () => { browser = await launchChromium(); });
  afterAll(async () => { await browser?.close(); });
  return base.extend<{ page: Page; watch: PageWatch }>(pageFixtures(() => browser, where, inject("csrArtifacts")));
}

for (const where of targets()) describe(`csr-todo on ${where.name}`, () => {
  const test = browserTest(where);
  const todos = (page: Page) => page.locator("main > ul > li");

  test("the SPA mounts and lists the server todos it reads over listTodos", async ({ page }) => {
    await page.goto(where.uiUrl);
    await expect(page.locator("h1")).toHaveText("csr-todo");
    await expect(todos(page)).toHaveText(SERVER_TODOS);
    await expect(page.getByText(`${SERVER_TODOS.length} todos`, { exact: true })).toBeVisible();
  });

  test("Add local appends a todo the page holds itself", async ({ page }) => {
    await page.goto(where.uiUrl);
    await expect(todos(page)).toHaveText(SERVER_TODOS);
    await page.getByRole("button", { name: "Add local" }).click();
    await expect(todos(page)).toHaveText([...SERVER_TODOS, "Local todo 5"]);
    await expect(page.getByText("5 todos", { exact: true })).toBeVisible();
  });

  test("a reload drops the local todo and reads the server todos again", async ({ page }) => {
    await page.goto(where.uiUrl);
    await expect(todos(page)).toHaveText(SERVER_TODOS);
    await page.getByRole("button", { name: "Add local" }).click();
    await expect(todos(page)).toHaveCount(SERVER_TODOS.length + 1);
    await page.reload();
    await expect(todos(page)).toHaveText(SERVER_TODOS);
  });

  test("the searchTodos stream delivers every match for the default and a typed query, then ends", async ({ page }) => {
    await page.goto(where.uiUrl);
    const matches = page.locator("section ul li");
    const status = page.locator("section p");
    await expect(matches).toHaveText(["Build the CSR demo"]);
    await expect(status).toHaveText("1 matches");
    await page.locator("section input").fill("the");
    await expect(matches).toHaveText(THE_MATCHES);
    await expect(status).toHaveText(`${THE_MATCHES.length} matches`);
  });
});

describe("csr-todo deployed through the gateway", () => {
  const deployed = target("deployed");
  const test = browserTest(deployed);

  test("the shell the gateway serves holds no app UI for a browser that runs no JavaScript", async ({ page }) => {
    const context = await page.context().browser()!.newContext({ javaScriptEnabled: false });
    try {
      const shell = await context.newPage();
      const response = await shell.goto(deployed.uiUrl);
      expect(response?.status()).toBe(200);
      await expect(shell.locator("#root")).toBeAttached();
      await expect(shell.locator("#root")).toBeEmpty();
      await expect(shell.locator("h1")).toHaveCount(0);
    } finally {
      await context.close();
    }
  });

  test("a deep route the bundle holds no asset for serves the shell, and the client router mounts the app", async ({ page }) => {
    const response = await page.goto(`${deployed.uiUrl}/some/spa/route`);
    expect(response?.status()).toBe(200);
    await expect(page.locator("h1")).toHaveText("csr-todo");
    await expect(page.getByText("Search stream")).toBeVisible();
    await expect(page.getByText(`${SERVER_TODOS.length} todos`, { exact: true })).toBeVisible();
  });

  test("the stream renders its matches one frame at a time", async ({ page }) => {
    await page.goto(deployed.uiUrl);
    const matches = page.locator("section ul li");
    await expect(matches).toHaveText(["Build the CSR demo"]);
    // Every commit React makes to the match list, as the count it leaves.
    await page.evaluate(() => {
      const list = document.querySelector("section ul");
      if (!list) throw new Error("the search section renders no list");
      const counts: number[] = [];
      (window as unknown as { matchCounts: number[] }).matchCounts = counts;
      new MutationObserver(() => counts.push(list.children.length)).observe(list, { childList: true });
    });
    await page.locator("section input").fill("the");
    await expect(matches).toHaveText(THE_MATCHES);
    await expect(page.locator("section p")).toHaveText(`${THE_MATCHES.length} matches`);
    const counts = await page.evaluate(() => (window as unknown as { matchCounts: number[] }).matchCounts);
    // At least one commit must have left a partial result: some, not all, of the matches.
    const partial = counts.filter((count) => count > 0 && count < THE_MATCHES.length);
    expect({ commits: counts, partial }).not.toEqual({ commits: counts, partial: [] });
  });
});
