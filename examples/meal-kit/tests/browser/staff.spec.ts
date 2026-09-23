import { visit } from "./helpers";
import AxeBuilder from "@axe-core/playwright";
import type { BrowserContext } from "@playwright/test";
import { test, expect } from "./fixtures";
import { signIn, rpc, raw, chooseOption } from "./helpers";
import { testOrigin } from "../fixture/settings";
import type { StaffMember, StaffSettings } from "@gather/meal-kit/staff-domain";
import type * as api from "../fixture/api";
import { defaultCart, type Order, type Quote } from "@gather/meal-kit/domain";
import { deliveryDates } from "@gather/meal-kit/catalog";
type Team = Awaited<ReturnType<typeof api.getStaffTeam>>;
type Workspace = Awaited<ReturnType<typeof api.getOperations>>;
type Catalog = Awaited<ReturnType<typeof api.getCatalogWorkspace>>;

async function updateAccess(admin: BrowserContext, subject: string, settings: Partial<StaffSettings>) {
  const team = await rpc<Team>(admin, "staffTeam", {}, true);
  const prior = team.members.find(member => member.subject === subject);
  return rpc<StaffMember>(admin, "saveStaffMember", {
    name: "Alex", recipeEditor: false, grants: [], ...prior, active: true, ...settings,
    subject, expectedVersion: prior?.version ?? null, requestKey: crypto.randomUUID(),
  });
}

test("administrators manage audited access while country editors see only their workspace", async ({ page, context, browser }) => {
  const colleague = await browser.newContext({ baseURL: testOrigin });
  let subject = "";
  try {
    await signIn(page, "ops@gather.example");
    const colleaguePage = await colleague.newPage();
    await signIn(colleaguePage, "alex@gather.example", false, "backoffice");
    subject = (await rpc<{ user: { id: string } }>(colleague, "session", {}, true)).user.id;
    expect((await raw(colleague, "staffTeam", {}, true)).status()).toBe(403);
    await visit(page, "/m/us/en/operations");
    await page.getByRole("tab", { name: "Team access", exact: true }).click();
    await page.getByRole("button", { name: "Add team member", exact: true }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel("Colleague's name", { exact: true }).fill("Alex menu editor");
    await dialog.getByLabel("Account ID", { exact: true }).fill(subject);
    await dialog.getByRole("group", { name: "United States", exact: true }).getByRole("checkbox", { name: "Menu editor", exact: true }).check();
    await page.setViewportSize({ width: 390, height: 844 });
    expect((await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21aa"]).analyze()).violations).toEqual([]);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await dialog.getByRole("button", { name: "Save access", exact: true }).click();
    await expect(dialog).not.toBeVisible();
    const team = await rpc<Team>(context, "staffTeam", {}, true);
    const member = team.members.find(member => member.subject === subject)!;
    expect(member).toMatchObject({ active: true, recipeEditor: false, grants: [{ market: "us", roles: ["menu_editor"] }] });
    expect(team.history).toEqual(expect.arrayContaining([expect.objectContaining({ subject, before: null, after: member })]));
    const command = { ...member, expectedVersion: member.version, requestKey: crypto.randomUUID(), name: "Alex editorial" };
    expect((await raw(colleague, "saveStaffMember", command)).status()).toBe(403);
    const saved = await rpc<StaffMember>(context, "saveStaffMember", command);
    expect(await rpc(context, "saveStaffMember", command)).toEqual(saved);
    expect((await raw(context, "saveStaffMember", { ...command, name: "Changed key reuse" })).status()).toBe(409);
    expect((await raw(context, "saveStaffMember", { ...command, requestKey: crypto.randomUUID() })).status()).toBe(409);
    expect((await raw(context, "saveStaffMember", { ...command, subject: team.administrators[0] })).status()).toBe(409);
    expect((await rpc<Team>(context, "staffTeam", {}, true)).history.filter(event => event.after.version === saved.version && event.subject === subject)).toHaveLength(1);
    await visit(colleaguePage, "/m/us/en/operations");
    await expect(colleaguePage.getByRole("tab", { name: "Menus & recipes", exact: true })).toBeVisible();
    for (const name of ["Orders & fulfillment", "Inventory & menu", "Support & refunds", "Team access", "Recipe library"])
      await expect(colleaguePage.getByRole("tab", { name, exact: true })).toHaveCount(0);
    expect(await rpc(colleague, "operations", { market: "us" }, true)).toMatchObject({ orders: [], stock: [], cases: [], recipeNames: [] });
    expect((await raw(colleague, "operations", { market: "cn" }, true)).status()).toBe(403);
    await chooseOption(colleaguePage.getByLabel("Delivery country", { exact: true }), "China");
    await expect(colleaguePage).toHaveURL(/\/m\/cn\/en\/operations$/);
    await expect(colleaguePage.getByRole("heading", { name: "Choose a country for your workspace", exact: true })).toBeVisible();
    await colleaguePage.getByRole("link", { name: "United States", exact: true }).click();
    await expect(colleaguePage).toHaveURL(/\/m\/us\/en\/operations$/);
    await updateAccess(context, subject, { active: false });
    expect((await raw(colleague, "catalogWorkspace", { market: "us" }, true)).status()).toBe(403);
    expect(await rpc(colleague, "session", {}, true)).toMatchObject({ user: { id: subject }, staff: null });
  } finally {
    if (subject) await updateAccess(context, subject, { active: false });
    await colleague.close();
  }
});

test("resource countries and action permissions protect orders, refunds and shared recipes", async ({ page, context, browser }) => {
  const colleague = await browser.newContext({ baseURL: testOrigin });
  // Buying belongs to the storefront and staff work to the back office, so the
  // order this test acts on is placed by a customer in the customer's app. The
  // back office never asks for it: it reads the same database.
  const shopper = await browser.newContext({ baseURL: testOrigin });
  let subject = "";
  try {
    await signIn(page, "ops@gather.example");
    await signIn(await colleague.newPage(), "alex@gather.example", false, "backoffice");
    await signIn(await shopper.newPage(), "sam@gather.example", false);
    subject = (await rpc<{ user: { id: string } }>(colleague, "session", {}, true)).user.id;
    const cart = { ...defaultCart("us"), deliveryDate: deliveryDates("us").at(-1)!, postal: "10001", recurring: false, mealCount: 2 as const, recipeIds: ["lemon-chicken", "pesto-pasta"] };
    const order = await rpc<Order>(shopper, "checkout", {
      cart, quote: await rpc<Quote>(shopper, "quote", cart), requestKey: crypto.randomUUID(), consent: true,
      address: { country: "US", province: "NY", district: "", name: "Sam Chen", email: "sam@gather.example", line: "12 Garden Street", city: "New York", postal: cart.postal, phone: "+12125550123", instructions: "" },
    });
    const issue = await rpc<{ id: string }>(shopper, "issue", { orderId: order.id, category: "delivery", message: "Please help with this delivery." });
    const us = await rpc<Workspace>(context, "operations", { market: "us" }, true);
    const cn = await rpc<Workspace>(context, "operations", { market: "cn" }, true);
    const stock = us.stock.find(row => row.available > 0)!;
    expect(stock).toBeDefined();
    expect(cn.stock.length).toBeGreaterThan(0);
    const inventory = { stockKey: stock.stock_key, available: stock.available, published: stock.published };
    const resolve = { id: issue.id, resolution: "The customer received delivery guidance.", refund: 1, requestKey: crypto.randomUUID() };
    await updateAccess(context, subject, { grants: [{ market: "cn", roles: ["manager"] }] });
    for (const [name, input] of [["inventory", inventory], ["advance", { id: order.id, next: "packing" }], ["resolve", resolve]] as const)
      expect((await raw(colleague, name, input)).status()).toBe(403);
    expect((await rpc<Workspace>(context, "operations", { market: "us" }, true)).orders.find(row => row.id === order.id)).toMatchObject({ fulfillment: "unallocated", refunded: 0 });
    await updateAccess(context, subject, { grants: [{ market: "us", roles: ["fulfillment"] }] });
    await rpc(colleague, "inventory", inventory);
    await rpc(colleague, "advance", { id: order.id, next: "packing" });
    const otherStock = cn.stock[0];
    expect((await raw(colleague, "inventory", { stockKey: otherStock.stock_key, available: otherStock.available, published: otherStock.published })).status()).toBe(403);
    expect((await raw(colleague, "resolve", { ...resolve, refund: 0 })).status()).toBe(403);
    expect((await raw(colleague, "catalogWorkspace", { market: "us" }, true)).status()).toBe(403);
    await updateAccess(context, subject, { grants: [{ market: "us", roles: ["support"] }] });
    expect((await rpc<Workspace>(colleague, "operations", { market: "us" }, true)).cases.map(row => row.id)).toContain(issue.id);
    expect((await raw(colleague, "resolve", resolve)).status()).toBe(403);
    await rpc(colleague, "resolve", { ...resolve, refund: 0 });
    const catalog = await rpc<Catalog>(context, "catalogWorkspace", { market: "us" }, true);
    const draft = catalog.recipes[0];
    const input = { id: draft.id, version: draft.version, slug: draft.slug, draft: draft.draft };
    await updateAccess(context, subject, { grants: [{ market: "us", roles: ["menu_editor"] }] });
    expect((await raw(colleague, "saveRecipeDraft", input)).status()).toBe(403);
    expect(await rpc(colleague, "catalogWorkspace", { market: "us" }, true)).toMatchObject({ canEditRecipes: false, canEditMenus: true });
    await updateAccess(context, subject, { grants: [], recipeEditor: true });
    await rpc(colleague, "saveRecipeDraft", input);
    expect(await rpc(colleague, "catalogWorkspace", { market: "cn" }, true)).toMatchObject({ canEditRecipes: true, canEditMenus: false, menus: [] });
    const menu = catalog.menus[0];
    expect((await raw(colleague, "withdrawMenu", { id: menu.id, version: menu.version })).status()).toBe(403);
  } finally {
    if (subject) await updateAccess(context, subject, { active: false, recipeEditor: false, grants: [] });
    await colleague.close();
    await shopper.close();
  }
});
