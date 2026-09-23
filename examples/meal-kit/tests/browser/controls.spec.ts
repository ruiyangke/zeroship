import { visit } from "./helpers";
import AxeBuilder from "@axe-core/playwright";
import { test, expect } from "./fixtures";
import { chooseOption, rpc } from "./helpers";
import { deliveryDates, money } from "@gather/meal-kit/catalog";
import type * as api from "../fixture/api";

test("box controls support keyboard sizing and aligned, available delivery days on mobile", async ({
  page,
  context,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  const catalog = await rpc<Awaited<ReturnType<typeof api.getCatalog>>>(
    context,
    "catalog",
    { market: "us", date: deliveryDates("us")[0] },
    true,
  );
  expect(catalog.dates.length).toBeGreaterThan(1);
  await visit(page, "/m/us/en/plans");
  await page.getByLabel("ZIP code").fill("10001");
  await page
    .getByRole("button", { name: "Continue to box size", exact: true })
    .click();
  const slider = page.getByRole("slider", { name: "People per meal" });
  await slider.press("End");
  await expect(slider).toHaveAttribute("aria-valuenow", "10");
  await expect(
    page.getByRole("button", { name: "One more person" }),
  ).toBeDisabled();
  await expect(
    page.locator(".wizard-mobile-summary .summary-total"),
  ).toContainText(
    money(catalog.menu!.price * 10 * 3 + catalog.menu!.shipping, "us", "en"),
  );
  await slider.press("Home");
  await expect(slider).toHaveAttribute("aria-valuetext", "1 person");
  await expect(
    page.getByRole("button", { name: "One fewer person" }),
  ).toBeDisabled();
  await slider.press("ArrowRight");
  await expect(slider).toHaveAttribute("aria-valuenow", "2");

  const grid = page.getByRole("grid", { name: "Delivery date", exact: true });
  await expect(
    grid.getByRole("columnheader", { includeHidden: true }),
  ).toHaveCount(7);
  const widths = await grid
    .getByRole("row")
    .evaluateAll((rows) =>
      rows.map((row) =>
        [...row.children].map((cell) => cell.getBoundingClientRect().width),
      ),
    );
  expect(widths.length).toBeGreaterThan(1);
  for (const row of widths) {
    expect(row).toHaveLength(7);
    expect(Math.max(...row) - Math.min(...row)).toBeLessThan(1);
  }
  const bounds = await grid.boundingBox();
  expect(bounds!.width).toBeLessThan(340);
  await expect(
    grid.getByRole("button", { name: /unavailable$/ }).first(),
  ).toBeDisabled();
  const nextDay = grid
    .getByRole("button", { name: /available for delivery$/ })
    .first();
  const day = (await nextDay.getAttribute("aria-label"))!.replace(
    ", available for delivery",
    "",
  );
  await nextDay.click();
  await expect(
    grid.getByRole("button", { name: `${day}, selected`, exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "See available meals" }),
  ).toBeEnabled();
  await chooseOption(
    page.getByRole("combobox", { name: "Language", exact: true }),
    /中文/,
  );
  await expect(
    page.getByRole("heading", { name: "搭配你的食材箱" }),
  ).toBeVisible();
  await expect(
    page.getByRole("grid", { name: "配送日期", exact: true }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: "tests/.artifacts/shadcn-wizard-mobile.png",
    fullPage: true,
  });
  expect(
    (
      await new AxeBuilder({ page })
        .withTags(["wcag2a", "wcag2aa", "wcag21aa"])
        .analyze()
    ).violations,
  ).toEqual([]);
});

test("shadcn selects preserve accessible names, keyboard choices, and dependent China address fields", async ({
  page,
}) => {
  await visit(page, "/m/us/en/plans");
  const country = page.getByRole("combobox", { name: "Delivery country" });
  await country.focus();
  await country.press("Enter");
  await page.getByRole("option", { name: "China", exact: true }).focus();
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/\/m\/cn\/en\/plans$/);
  const province = page.getByRole("combobox", {
    name: "Province or municipality",
    exact: true,
  });
  const city = page.getByRole("combobox", { name: "City", exact: true });
  const district = page.getByRole("combobox", {
    name: "District",
    exact: true,
  });
  await expect(city).toBeDisabled();
  await expect(district).toBeDisabled();
  await chooseOption(province, "Shanghai");
  await chooseOption(city, "Shanghai");
  await chooseOption(district, "Pudong");
  await expect(district).toContainText("Pudong");
  await province.click();
  await expect(page.getByRole("listbox")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(province).toBeFocused();
  await expect(page.getByRole("option")).toHaveCount(0);
  await chooseOption(province, "Other province or municipality");
  await expect(city).toContainText("Select city");
  await expect(district).toBeDisabled();
  await expect(page.getByLabel(/ZIP code|Postal code/i)).toHaveCount(0);
});
