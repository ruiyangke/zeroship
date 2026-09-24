import { visit } from "./helpers";
import { seedBox, loadBox } from "./helpers";
import { chooseOption } from "./helpers";
import AxeBuilder from "@axe-core/playwright";
import { test, expect } from "./fixtures";
import { signIn } from "./helpers";
import { defaultCart } from "@gather/meal-kit/domain";

test("a shopper can review an incomplete box, change its size without losing meals and return after sign-in", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await visit(page, "/");
  await expect(
    page.getByRole("heading", { name: "Where will you be cooking?" }),
  ).toBeVisible();
  await page.getByRole("link", { name: "United States New York" }).click();
  await visit(page, "/");
  await expect(page).toHaveURL(/\/m\/us\/en$/);
  await visit(page, "/m/us/en/plans");
  await page.getByLabel("ZIP code").fill("10001");
  await page
    .getByRole("button", { name: "Continue to box size", exact: true })
    .click();
  await page
    .getByRole("group", { name: "Delivery frequency" })
    .getByRole("radio", { name: /One-time box/ })
    .check();
  await page.getByRole("button", { name: "See available meals" }).click();
  await page
    .getByRole("button", { name: "Add Lemon & herb chicken", exact: true })
    .click();
  await page
    .locator(".mobile-box")
    .getByRole("link", { name: "Your box" })
    .click();
  await expect(page).toHaveURL(/\/box$/);
  await expect(page.locator("#main")).toBeFocused();
  await expect(page.locator(".box-summary")).toContainText(
    "One-time purchase. No recurring deliveries.",
  );
  await expect(
    page.getByRole("link", { name: "Continue to checkout" }),
  ).toHaveCount(0);
  await page.getByRole("link", { name: "Continue choosing meals" }).click();
  await expect(page.locator("#choose-meals")).toBeFocused();
  await page
    .getByRole("button", { name: "Add Garden pesto rigatoni", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Add Miso-glazed salmon", exact: true })
    .click();
  await page.getByRole("link", { name: "Delivery & box size" }).click();
  await page
    .getByRole("group", { name: "Meals per box" })
    .getByRole("radio", { name: "2 meals", exact: true })
    .click();
  await expect(
    page.getByText(
      "Your meals are still selected. Review your box to choose which ones to keep.",
    ),
  ).toBeVisible();
  await page
    .getByRole("link", { name: "Review your box", exact: true })
    .click();
  await expect(page.locator(".box-slot.filled")).toHaveCount(3);
  await expect(
    page.getByRole("link", { name: "Continue to checkout" }),
  ).toHaveCount(0);
  await page
    .getByRole("button", { name: "Remove Miso-glazed salmon", exact: true })
    .click();
  await page.screenshot({
    path: "tests/.artifacts/box-review-mobile.png",
    fullPage: true,
  });
  expect(
    (
      await new AxeBuilder({ page })
        .withTags(["wcag2a", "wcag2aa", "wcag21aa"])
        .analyze()
    ).violations,
  ).toEqual([]);
  await page.getByRole("link", { name: "Continue to checkout" }).click();
  const opened = page.waitForEvent("popup");
  await page.getByRole("button", { name: "Sign in to continue" }).click();
  const popup = await opened;
  await popup.getByLabel("Dev user").selectOption("alex@gather.example");
  await popup.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Review your order" }),
  ).toBeVisible();
  await expect(page.locator(".box-slot.filled")).toHaveCount(2);
  await chooseOption(page.getByLabel("Language", { exact: true }), /中文/);
  await expect(page.getByRole("heading", { name: "确认订单" })).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
});

test("stale dates and unavailable meals stay visible until the shopper repairs them", async ({
  page,
}) => {
  await visit(page, "/m/us/en");
  const cart = {
    ...defaultCart("us"),
    postal: "10001",
    recipeIds: ["lemon-chicken", "pesto-pasta", "miso-salmon"],
  };
  await seedBox(page.context(), cart);
  await page.route(
    (url) => url.pathname.endsWith("/gather.catalog"),
    async (route) => {
      const response = await route.fetch();
      const body = await response.json();
      body.json.menu.recipes = body.json.menu.recipes.filter(
        (recipe: { id: string }) => recipe.id !== "miso-salmon",
      );
      body.json.recipes = body.json.menu.recipes;
      await route.fulfill({ response, json: body });
    },
  );
  await visit(page, "/m/us/en/box");
  await expect(
    page.getByText("No longer on this menu", { exact: true }),
  ).toBeVisible();
  await expect(page.locator(".box-slot.filled")).toHaveCount(3);
  await expect(
    page.getByRole("link", { name: "Continue to checkout" }),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "Remove unavailable meal" }).click();
  await expect(page.locator(".box-slot.filled")).toHaveCount(2);
  await page.unrouteAll({ behavior: "wait" });
  await expect
    .poll(
      async () =>
        (await loadBox(page.context())).state.draft?.cart.recipeIds.length,
    )
    .toBe(2);
  await seedBox(page.context(), { ...cart, deliveryDate: "2020-01-01" });
  await page.reload();
  await expect(
    page.getByText(
      "This delivery date is no longer available. Choose a new date, then review your meals.",
    ),
  ).toBeVisible();
  await page.getByRole("link", { name: "Choose delivery details" }).click();
  await page
    .getByRole("button", { name: "Continue to box size", exact: true })
    .click();
  const calendar = page.getByRole("grid", { name: "Delivery date", exact: true });
  // Days are offered; none of them is this box's stale date. Counting the
  // selected cells without counting the cells first passes on a step that has
  // no calendar at all, which is what a wizard that failed to advance looks
  // like.
  await expect(calendar.getByRole("gridcell")).not.toHaveCount(0);
  await expect(calendar.getByRole("gridcell", { selected: true })).toHaveCount(
    0,
  );
  await expect(
    page.getByText(
      "Your previous delivery date is no longer available. Choose a new date to continue.",
    ),
  ).toBeVisible();
  // Changing country reloads the box for the new market over the network, so
  // from here the shopper is looking at a delivery step whose saved values are
  // still in flight. Hold that load open: without it this passes or fails on
  // how busy the machine is, and the suite is where it gets busy.
  await page.route("**/api/drafts/load", async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 1_000));
    await route.continue();
  });
  await chooseOption(page.getByLabel("Delivery country"), "China");
  await expect(
    page.getByLabel("Province or municipality", { exact: true }),
  ).toBeVisible();
  await expect(page.getByLabel(/ZIP|postal|postcode/i)).toHaveCount(0);
  await chooseOption(page.getByLabel("Delivery country"), "United States");
  // The saved US delivery area comes back after the round trip. Clicking
  // before it does submits an empty required field, which the browser refuses
  // without ever reaching the wizard.
  await expect(page.getByLabel("ZIP code")).toHaveValue("10001");
  await page
    .getByRole("button", { name: "Continue to box size", exact: true })
    .click();
  await expect(calendar.getByRole("gridcell")).not.toHaveCount(0);
  await expect(calendar.getByRole("gridcell", { selected: true })).toHaveCount(
    0,
  );
  await expect(
    page.getByRole("button", { name: "See available meals" }),
  ).toBeDisabled();
});

test("allergen filters keep their exclusions, mobile navigation restores focus, and missing pages have recovery", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await visit(page, "/m/us/en/menu");
  await page.getByRole("button", { name: "Allergens", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByRole("checkbox", { name: "Milk", exact: true }).check();
  await dialog.getByRole("button", { name: "Show meals" }).click();
  await page.getByLabel("Search recipes").fill("no match for this meal");
  await page.getByRole("button", { name: "Clear search and category" }).click();
  // The meal that survives the exclusion first: without it, "the excluded meal
  // is gone" also holds on a list that has not come back from the search yet.
  await expect(
    page.getByRole("heading", { name: "Miso-glazed salmon", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Garden pesto rigatoni", exact: true }),
  ).toHaveCount(0);
  const trigger = page.getByRole("button", { name: "Open navigation" });
  await trigger.click();
  await expect(
    dialog.getByRole("heading", { name: "Explore Gather" }),
  ).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(dialog).not.toBeVisible();
  await expect(trigger).toBeFocused();
  await visit(page, "/m/us/en/this-page-does-not-exist");
  await expect(
    page.getByRole("heading", { name: "We couldn't find that page" }),
  ).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Good questions. Simple answers." }),
  ).toHaveCount(0);
});

test("checkout follows the entered destination and a changed total requires new consent", async ({
  page,
}) => {
  await signIn(page, "sam@gather.example");
  const cart = {
    ...defaultCart("us"),
    postal: "10001",
    recipeIds: ["lemon-chicken", "pesto-pasta", "miso-salmon"],
  };
  await seedBox(page.context(), cart);
  let extra = 0;
  const destinations: string[] = [];
  await page.route("**/gather.quote", async (route) => {
    const input = route.request().postDataJSON();
    destinations.push(input.postal);
    const response = await route.fetch();
    const body = await response.json();
    body.json.total += extra;
    body.json.subtotal += extra;
    await route.fulfill({ response, json: body });
  });
  await visit(page, "/m/us/en/checkout");
  await page.getByLabel("Street address").fill("51 Dinner Street");
  await page.getByLabel("Phone number").fill("+12125550123");
  await expect(page.locator(".summary-total")).toContainText("$");
  const consent = page.getByRole("checkbox");
  await consent.check();
  await expect(
    page.getByRole("button", { name: "Place order", exact: true }),
  ).toBeEnabled();
  await page.getByLabel("ZIP code", { exact: true }).fill("10003");
  await expect(consent).not.toBeChecked();
  await expect.poll(() => destinations.at(-1)).toBe("10003");
  await expect(page.getByLabel("Street address")).toHaveValue(
    "51 Dinner Street",
  );
  await expect(page.locator(".summary-total")).toContainText("$");
  await consent.check();
  const oldTotal = await page.locator(".summary-total").innerText();
  extra = 100;
  await page.getByRole("button", { name: "Refresh total" }).click();
  await expect(page.locator(".summary-total")).not.toHaveText(oldTotal);
  await expect(consent).not.toBeChecked();
  await expect(
    page.getByRole("button", { name: "Place order", exact: true }),
  ).toBeDisabled();
  await consent.check();
  await expect(
    page.getByRole("button", { name: "Place order", exact: true }),
  ).toBeEnabled();
  await visit(page, "/m/us/en/account?view=history");
  await expect(
    page.getByRole("tab", { name: "Order history", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await page.reload();
  await expect(
    page.getByRole("tab", { name: "Order history", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("tabpanel")).toHaveCount(1);
  await expect(
    page.getByRole("tab", { name: "Upcoming boxes", exact: true }),
  ).toHaveAttribute("aria-selected", "false");
});
