import { visit } from "./helpers";
import type { Page } from "@playwright/test";
import { test, expect } from "./fixtures";
import { seedBox, signIn } from "./helpers";
import { defaultCart } from "@gather/meal-kit/domain";

type PurchaseBounds = {
  left: number;
  width: number;
  primaryWidth: number;
  asideLeft: number | null;
  asideWidth: number | null;
};

async function measureStep(page: Page, layoutSelector: string) {
  const layout = page.locator(layoutSelector);
  await expect(layout).toBeVisible();
  await expect
    .poll(() =>
      page
        .locator("#main")
        .evaluate(
          (main) =>
            main
              .getAnimations({ subtree: true })
              .filter(
                (animation) =>
                  animation.playState === "running" &&
                  animation.effect?.getComputedTiming().iterations !== Infinity,
              ).length,
        ),
    )
    .toBe(0);
  const measured = await layout.evaluate((element) => {
    const section = element.closest("section")!;
    const progress = section.querySelector(".purchase-steps")!;
    const heading = section.querySelector("h1")!;
    const bounds = element.getBoundingClientRect();
    const progressBounds = progress.getBoundingClientRect();
    const primary = element.firstElementChild!.getBoundingClientRect();
    const aside = element.lastElementChild!.getBoundingClientRect();
    return {
      left: bounds.left,
      width: bounds.width,
      primaryWidth: primary.width,
      asideLeft: aside.width ? aside.left : null,
      asideWidth: aside.width || null,
      headingLeft: heading.getBoundingClientRect().left,
      progressLeft: progressBounds.left,
      progressWidth: progressBounds.width,
      overflow:
        document.documentElement.scrollWidth >
        document.documentElement.clientWidth,
    };
  });
  expect(measured.left).toBeCloseTo(measured.progressLeft, 0);
  expect(measured.width).toBeCloseTo(measured.progressWidth, 0);
  expect(measured.headingLeft).toBeCloseTo(measured.left, 0);
  expect(measured.overflow).toBe(false);
  expect(measured.primaryWidth).toBeGreaterThan(0);
  return measured;
}

for (const width of [1440, 820, 390, 320]) {
  test(`purchase steps keep their content and columns aligned at viewport ${width}`, async ({
    page,
  }) => {
    await signIn(page, "sam@gather.example");
    await page.setViewportSize({ width, height: 900 });
    await seedBox(page.context(), {
      ...defaultCart("us"),
      postal: "10001",
      recipeIds: ["lemon-chicken", "pesto-pasta", "miso-salmon"],
    });
    await visit(page, "/m/us/en/plans");
    await expect(page.getByLabel("ZIP code")).toHaveValue("10001");
    const delivery = await measureStep(page, ".wizard-layout");
    const compare = (step: PurchaseBounds) => {
      expect(step.left).toBeCloseTo(delivery.left, 0);
      expect(step.width).toBeCloseTo(delivery.width, 0);
      expect(step.primaryWidth).toBeCloseTo(delivery.primaryWidth, 0);
      if (width > 760) {
        expect(step.asideLeft).toBeCloseTo(delivery.asideLeft!, 0);
        expect(step.asideWidth).toBeCloseTo(delivery.asideWidth!, 0);
      } else {
        expect(step.primaryWidth).toBeCloseTo(step.width, 0);
        if (step.asideWidth !== null) {
          expect(step.asideLeft).toBeCloseTo(step.left, 0);
          expect(step.asideWidth).toBeCloseTo(step.width, 0);
        }
      }
    };
    await page.getByRole("button", { name: "Continue to box size" }).click();
    await expect(
      page.getByRole("heading", { name: "Choose your box." }),
    ).toBeVisible();
    compare(await measureStep(page, ".wizard-layout"));
    const calendar = await page.locator(".delivery-calendar").boundingBox();
    const grid = await page
      .getByRole("grid", { name: "Delivery date", exact: true })
      .boundingBox();
    expect(grid!.x).toBeGreaterThanOrEqual(calendar!.x);
    expect(grid!.x + grid!.width).toBeLessThanOrEqual(
      calendar!.x + calendar!.width,
    );
    const form = await page.locator(".wizard-form").boundingBox();
    const actions = await page
      .locator(".wizard-actions > *")
      .evaluateAll((elements) =>
        elements.map((element) => element.getBoundingClientRect().right),
      );
    for (const right of actions)
      expect(right).toBeLessThanOrEqual(form!.x + form!.width);
    await page.getByRole("button", { name: "See available meals" }).click();
    await expect(
      page.getByRole("heading", { name: "What sounds good this week?" }),
    ).toBeVisible();
    compare(await measureStep(page, ".menu-layout"));
    const boxLink =
      width > 760
        ? page
            .locator(".menu-layout > .box-summary")
            .getByRole("link", { name: "Review your box", exact: true })
        : page
            .locator(".mobile-box")
            .getByRole("link", { name: "Your box", exact: true });
    await boxLink.click();
    await expect(
      page.getByRole("heading", { name: "Review your box", exact: true }),
    ).toBeVisible();
    compare(await measureStep(page, ".box-review"));
    await page.getByRole("link", { name: "Continue to checkout" }).click();
    await expect(
      page.getByRole("heading", { name: "Review your order" }),
    ).toBeVisible();
    await expect(page.getByLabel("Street address")).toBeVisible();
    compare(await measureStep(page, ".checkout-grid"));
  });
}
