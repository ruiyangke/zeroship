import { visit } from "./helpers";
import type { Page } from "@playwright/test";
import { test, expect } from "./fixtures";
import { chooseOption, signIn } from "./helpers";

type Sample = { x: number; y: number; opacity: number };
declare global {
  interface Window {
    gatherMotionFrames: Record<string, Sample[]>;
    gatherMotionFrame: number;
  }
}

async function trackMotion(page: Page) {
  await page.evaluate(() => {
    cancelAnimationFrame(window.gatherMotionFrame);
    window.gatherMotionFrames = { page: [], wizard: [], panel: [] };
    const sample = () => {
      for (const kind of Object.keys(window.gatherMotionFrames)) {
        const node = document.querySelector(`[data-transition="${kind}"]`);
        if (!node) continue;
        const style = getComputedStyle(node);
        const matrix = new DOMMatrixReadOnly(style.transform);
        window.gatherMotionFrames[kind].push({
          x: matrix.m41,
          y: matrix.m42,
          opacity: Number(style.opacity),
        });
      }
      window.gatherMotionFrame = requestAnimationFrame(sample);
    };
    sample();
  });
}

async function settled(page: Page) {
  await expect
    .poll(() =>
      page.locator("[data-transition]").evaluateAll((nodes) =>
        nodes.every((node) => {
          const style = getComputedStyle(node);
          return style.transform === "none" && style.opacity === "1";
        }),
      ),
    )
    .toBe(true);
}

for (const preference of ["no-preference", "reduce"] as const) {
  test(`page and wizard transitions respect ${preference} and keep one usable view`, async ({
    page,
  }) => {
    await page.emulateMedia({ reducedMotion: preference });
    await visit(page, "/m/us/en");
    await settled(page);
    const header = await page.locator("header").boundingBox();
    await trackMotion(page);
    await page.getByRole("link", { name: "Our plans", exact: true }).click();
    await expect(page.getByLabel("ZIP code")).toBeVisible();
    await expect(page.locator("#main")).toBeFocused();
    await settled(page);
    expect(await page.locator("header").boundingBox()).toEqual(header);
    const pageFrames = await page.evaluate(
      () => window.gatherMotionFrames.page,
    );
    expect(pageFrames.some((sample) => sample.opacity < 1)).toBe(
      preference === "no-preference",
    );
    await page.getByLabel("ZIP code").fill("10001");
    await trackMotion(page);
    await page.getByRole("button", { name: "Continue to box size" }).click();
    await expect(
      page.getByRole("heading", { name: "Choose your box." }),
    ).toBeFocused();
    await settled(page);
    const forward = await page.evaluate(() => window.gatherMotionFrames);
    expect(forward.wizard.some((sample) => sample.x > 0)).toBe(
      preference === "no-preference",
    );
    expect(
      forward.page.every((sample) => sample.x === 0 && sample.y === 0),
    ).toBe(true);
    await trackMotion(page);
    await page.getByRole("link", { name: "Back", exact: true }).click();
    await expect(page.getByLabel("ZIP code")).toHaveValue("10001");
    await expect(
      page.getByRole("heading", { name: "Where should we deliver?" }),
    ).toBeFocused();
    await settled(page);
    const backward = await page.evaluate(
      () => window.gatherMotionFrames.wizard,
    );
    expect(backward.some((sample) => sample.x < 0)).toBe(
      preference === "no-preference",
    );
    await page.evaluate(() => {
      document
        .querySelector<HTMLAnchorElement>('.desktop-nav a[href$="/menu"]')!
        .click();
      document
        .querySelector<HTMLAnchorElement>('.desktop-nav a[href$="/help"]')!
        .click();
    });
    await expect(
      page.getByRole("heading", { name: "Good questions. Simple answers." }),
    ).toBeVisible();
    await settled(page);
    await expect(page.locator("#main h1")).toHaveCount(1);
    await expect(page.locator("#main")).toBeFocused();
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
  });
}

test("back restores reading position while language changes preserve the current control", async ({
  page,
}) => {
  await visit(page, "/m/us/en/menu");
  await expect(
    page.getByRole("heading", { name: "Lemon & herb chicken", exact: true }),
  ).toBeVisible();
  await settled(page);
  await page.evaluate(() => scrollTo({ top: 450, behavior: "instant" }));
  await expect.poll(() => page.evaluate(() => scrollY)).toBe(450);
  await page
    .locator('.desktop-nav a[href$="/help"]')
    .evaluate((node: HTMLAnchorElement) => node.click());
  await expect(
    page.getByRole("heading", { name: "Good questions. Simple answers." }),
  ).toBeVisible();
  await expect.poll(() => page.evaluate(() => scrollY)).toBe(0);
  await page.goBack();
  await expect(page.getByLabel("Search recipes")).toBeVisible();
  await expect.poll(() => page.evaluate(() => scrollY)).toBe(450);
  await settled(page);
  const search = page.getByLabel("Search recipes");
  await search.fill("lemon");
  await search.evaluate((node) =>
    node.setAttribute("data-preserved-control", "true"),
  );
  await trackMotion(page);
  await chooseOption(page.getByLabel("Language", { exact: true }), /中文/);
  await expect(page.locator("html")).toHaveAttribute("lang", "zh-CN");
  await expect(page.locator('[data-preserved-control="true"]')).toHaveValue(
    "lemon",
  );
  expect(
    (await page.evaluate(() => window.gatherMotionFrames.page)).every(
      (sample) => sample.x === 0 && sample.y === 0 && sample.opacity === 1,
    ),
  ).toBe(true);
});

test("account tabs animate only their panel and retain keyboard focus", async ({
  page,
}) => {
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await signIn(page);
  await visit(page, "/m/us/en/account");
  const history = page.getByRole("tab", { name: "Order history", exact: true });
  await expect(history).toBeVisible();
  await settled(page);
  await trackMotion(page);
  await history.click();
  await expect(history).toBeFocused();
  await expect(history).toHaveAttribute("aria-selected", "true");
  await settled(page);
  const frames = await page.evaluate(() => window.gatherMotionFrames);
  expect(frames.panel.some((sample) => sample.y > 0)).toBe(true);
  expect(
    frames.page.every((sample) => sample.y === 0 && sample.opacity === 1),
  ).toBe(true);
});

test("changing country on the delivery step focuses the new country's content", async ({
  page,
}) => {
  await visit(page, "/m/us/en/plans");
  await expect(page.getByLabel("ZIP code")).toBeVisible();
  await chooseOption(
    page.getByLabel("Delivery country", { exact: true }),
    "China",
  );
  await expect(page).toHaveURL("/m/cn/en/plans");
  await expect(
    page.getByLabel("Province or municipality", { exact: true }),
  ).toBeVisible();
  await expect(page.locator("#main")).toBeFocused();
  await expect.poll(() => page.evaluate(() => scrollY)).toBe(0);
  await settled(page);
});
