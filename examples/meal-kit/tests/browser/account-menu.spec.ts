import { visit } from "./helpers";
import AxeBuilder from "@axe-core/playwright";
import { test, expect } from "./fixtures";
import { defaultCart } from "@gather/meal-kit/domain";
import { loadBox, rpc, seedBox, signIn } from "./helpers";

test("header account menu supports keyboard dismissal and direct account navigation", async ({
  page,
}) => {
  await signIn(page);
  const trigger = page
    .locator("header")
    .getByRole("button", { name: "My account", exact: true });
  const menu = page.getByRole("menu", { name: "My account", exact: true });
  await expect(trigger).toBeInViewport();
  const footer = page.locator("footer");
  // Signing out belongs to the header menu, and the footer this customer does
  // get is the control: without it, "not in the footer" also passes on a page
  // with no footer.
  await expect(footer.getByRole("link", { name: "Manage your plan" })).toBeVisible();
  await expect(footer.getByText("Sign out", { exact: true })).toHaveCount(0);
  // The one staff entry point the storefront can render is this footer link,
  // shown only for a session with staff access. A customer gets none.
  await expect(
    footer.getByRole("link", { name: "Operations", exact: true }),
  ).toHaveCount(0);
  await trigger.focus();
  await trigger.press("ArrowDown");
  await expect(menu).toBeVisible();
  await expect(
    menu.getByRole("menuitem", { name: "My deliveries", exact: true }),
  ).toBeFocused();
  await menu.press("End");
  await expect(
    menu.getByRole("menuitem", { name: "Sign out", exact: true }),
  ).toBeFocused();
  await menu.press("Escape");
  await expect(menu).not.toBeVisible();
  await expect(trigger).toBeFocused();

  for (const [label, route, heading] of [
    ["My deliveries", "account", "My deliveries"],
    ["Address book", "account/addresses", "Saved addresses"],
    ["Food preferences", "account/preferences", "Your food preferences"],
    ["Privacy & data", "account/privacy", "Privacy and data"],
  ]) {
    await trigger.click();
    await expect(menu.getByRole("menuitem")).not.toHaveCount(0);
    await expect(
      menu.getByRole("menuitem", { name: "Operations", exact: true }),
    ).toHaveCount(0);
    await menu.getByRole("menuitem", { name: label, exact: true }).click();
    await expect(page).toHaveURL(`/m/us/en/${route}`);
    await expect(
      page.getByRole("heading", { name: heading, exact: true }),
    ).toBeVisible();
    await expect(menu).not.toBeVisible();
    await expect(page.locator("main")).toBeFocused();
  }

  await trigger.click();
  const accessibility = await new AxeBuilder({ page })
    .include('[data-slot="dropdown-menu-content"]')
    .analyze();
  expect(accessibility.violations).toEqual([]);
});

test("mobile account menu signs out from the header and clears private box state", async ({
  page,
  context,
}) => {
  await signIn(page, "sam@gather.example");
  await seedBox(context, {
    ...defaultCart("us"),
    postal: "10001",
    servings: 6,
    recipeIds: ["lemon-chicken"],
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await visit(page, "/m/us/en/box");
  await expect(page.locator(".box-slot.filled")).toHaveCount(1);
  const trigger = page
    .locator("header")
    .getByRole("button", { name: "My account", exact: true });
  await expect(trigger).toBeInViewport();
  const triggerBounds = await trigger.boundingBox();
  expect(triggerBounds!.width).toBeGreaterThanOrEqual(44);
  expect(triggerBounds!.height).toBeGreaterThanOrEqual(44);
  await trigger.click();
  const menu = page.getByRole("menu", { name: "My account", exact: true });
  await expect(
    menu.getByText("sam@gather.example", { exact: true }),
  ).toBeVisible();
  await expect(
    menu.getByRole("menuitem", { name: "Sign out", exact: true }),
  ).toBeInViewport();
  const bounds = await menu.boundingBox();
  expect(bounds!.x).toBeGreaterThanOrEqual(0);
  expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(390);
  await menu
    .getByRole("menuitem", { name: "My deliveries", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "My deliveries", exact: true }),
  ).toBeVisible();
  await trigger.click();
  await menu.getByRole("menuitem", { name: "Sign out", exact: true }).click();
  await expect(trigger).not.toBeVisible();
  await expect
    .poll(
      async () =>
        (await rpc<{ user: unknown }>(context, "session", {}, true)).user,
    )
    .toBeNull();
  expect((await loadBox(context)).state.draft).toBeNull();
  await visit(page, "/m/us/en/box");
  // The box still has its slots; what it no longer has is the signed-out
  // customer's meals. Counting only the filled ones passes on a page that
  // never rendered a box.
  await expect(page.locator(".box-slot")).toHaveCount(3);
  await expect(page.locator(".box-slot.filled")).toHaveCount(0);
  await page
    .getByRole("button", { name: "Open navigation", exact: true })
    .click();
  await expect(
    page
      .getByRole("dialog")
      .getByRole("button", { name: "Log in", exact: true }),
  ).toBeVisible();
});
