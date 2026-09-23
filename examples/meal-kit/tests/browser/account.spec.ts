import { visit } from "./helpers";
import { chooseOption } from "./helpers";
import { testOrigin } from "../fixture/settings";
import { recipes } from "../../apps/backoffice/src/seed-catalog";
import { test, expect } from "./fixtures";
import AxeBuilder from "@axe-core/playwright";
import { signIn, rpc, raw } from "./helpers";
import { defaultCart, type Quote, type Order } from "@gather/meal-kit/domain";
import { markets, type MarketId } from "@gather/meal-kit/catalog";
import type * as api from "../fixture/api";

type Account = Awaited<ReturnType<typeof api.getAccount>>;
type SavedAddress = Account["addresses"][number];
const address = {
  country: "US",
  province: "NY",
  district: "",
  name: "Sam Chen",
  email: "sam@gather.example",
  line: "42 Kitchen Street",
  city: "New York",
  postal: "10001",
  phone: "+12125550123",
  instructions: "Ring the bell",
};

test("address book, preference persistence and checkout selection", async ({
  page,
  context,
  browser,
}) => {
  await signIn(page, "sam@gather.example");
  const previous = await rpc<Account>(
    context,
    "account",
    { market: "us" },
    true,
  );
  for (const row of previous.addresses)
    await rpc(context, "deleteAddress", { id: row.id, version: row.version });
  await visit(page, "/m/us/en/account/addresses");
  await page.getByRole("button", { name: "Add address", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await dialog
    .getByRole("button", { name: "Save address", exact: true })
    .click();
  await expect(dialog.getByRole("alert")).toHaveText(
    "Give this address a name, such as Home or Work.",
  );
  await expect(dialog.getByLabel("Address label")).toBeFocused();
  await dialog.getByLabel("Address label").fill("Home");
  await dialog.getByLabel("Street address").fill(address.line);
  await dialog.getByLabel("ZIP code", { exact: true }).fill(address.postal);
  await dialog.getByLabel("Phone number").fill(address.phone);
  await dialog
    .getByRole("button", { name: "Save address", exact: true })
    .click();
  await expect(dialog).not.toBeVisible();
  await expect(page.getByRole("article", { name: "Home" })).toContainText(
    "Default address",
  );
  const account = await rpc<Account>(
    context,
    "account",
    { market: "us" },
    true,
  );
  const home = account.addresses[0];
  const work = await rpc<SavedAddress>(context, "saveAddress", {
    market: "us",
    label: "Work",
    address: { ...address, line: "99 Work Street", postal: "10002" },
    isDefault: true,
  });
  expect(
    (await rpc<Account>(context, "account", { market: "us" }, true)).addresses
      .filter((row) => row.is_default)
      .map((row) => row.id),
  ).toEqual([work.id]);
  expect(
    (
      await raw(context, "saveAddress", {
        id: home.id,
        version: home.version,
        market: "us",
        label: "Stale edit",
        address,
        isDefault: true,
      })
    ).status(),
  ).toBe(409);
  expect(
    (
      await raw(context, "saveAddress", {
        market: "uk",
        label: "Wrong country",
        address,
        isDefault: false,
      })
    ).status(),
  ).toBe(400);
  const other = await browser.newContext({ baseURL: testOrigin });
  const otherPage = await other.newPage();
  await signIn(otherPage);
  expect(
    (
      await raw(other, "deleteAddress", { id: work.id, version: work.version })
    ).status(),
  ).toBe(404);
  expect(
    (
      await raw(other, "saveAddress", {
        id: work.id,
        version: work.version,
        market: "us",
        label: "Stolen",
        address,
        isDefault: true,
      })
    ).status(),
  ).toBe(404);
  await other.close();
  await visit(page, "/m/us/en/account/preferences");
  await page.getByRole("checkbox", { name: "Milk", exact: true }).check();
  await page
    .getByRole("checkbox", { name: "Mediterranean", exact: true })
    .check();
  await page.getByRole("checkbox", { name: "Oven", exact: true }).check();
  await page
    .getByRole("group", { name: "Cooking units" })
    .getByRole("radio", { name: "UK measures" })
    .check();
  await page
    .getByRole("button", { name: "Save preferences", exact: true })
    .click();
  await expect(
    page.getByRole("status").filter({ hasText: "Preferences saved." }),
  ).toBeVisible();
  await rpc(context, "preferences", { favorites: ["lemon-chicken"] });
  const profile = (
    await rpc<Account>(context, "account", { market: "us" }, true)
  ).profile;
  expect(profile.preferences).toMatchObject({
    favorites: ["lemon-chicken"],
    exclude: ["milk"],
    cuisines: ["mediterranean"],
    units: "imperial",
    marketing: false,
  });
  await page.reload();
  await expect(
    page.getByRole("checkbox", { name: "Milk", exact: true }),
  ).toBeChecked();
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
  await visit(page, "/m/us/en/plans");
  await page.getByLabel("ZIP code").fill("10001");
  await page
    .getByRole("button", { name: "Continue to box size", exact: true })
    .click();
  await page
    .getByRole("button", { name: "See available meals", exact: true })
    .click();
  for (const recipe of recipes.slice(0, 3))
    await page
      .getByRole("button", { name: `Add ${recipe.name}`, exact: true })
      .click();
  await page
    .getByRole("link", { name: "Review your box", exact: true })
    .click();
  await page.getByRole("link", { name: "Continue to checkout" }).click();
  await page
    .getByRole("group", { name: "Use a saved address" })
    .getByRole("radio", { name: "Work 99 Work Street" })
    .check();
  await expect(page.getByLabel("Street address")).toHaveValue("99 Work Street");
  await expect(page.getByLabel("ZIP code", { exact: true })).toHaveValue(
    "10002",
  );
  await expect(
    page.getByRole("button", { name: "Place order", exact: true }),
  ).toBeVisible();
  await rpc(context, "deleteAddress", { id: work.id, version: work.version });
  const remaining = await rpc<Account>(
    context,
    "account",
    { market: "us" },
    true,
  );
  expect(
    remaining.addresses.find((row) => row.id === home.id)?.is_default,
  ).toBe(true);
});

test("plans in different markets remain independent and reject stale control requests", async ({
  page,
  context,
}) => {
  await signIn(page, "sam@gather.example");
  const orders: Order[] = [];
  for (const market of ["us", "uk"] as MarketId[]) {
    const cart = {
      ...defaultCart(market),
      postal: markets[market].postal,
      recipeIds: recipes.slice(0, 3).map((recipe) => recipe.id),
    };
    const quote = await rpc<Quote>(context, "quote", cart);
    orders.push(
      await rpc<Order>(context, "checkout", {
        cart,
        address: {
          ...address,
          country: markets[market].country,
          postal: cart.postal,
          city: market === "us" ? "New York" : "London",
        },
        quote,
        requestKey: crypto.randomUUID(),
        outcome: "succeeded",
        consent: true,
      }),
    );
  }
  const us = await rpc<Account>(context, "account", { market: "us" }, true);
  const uk = await rpc<Account>(context, "account", { market: "uk" }, true);
  expect(us.plan.id).not.toBe(uk.plan.id);
  expect(us.orders.map((order) => order.id)).toContain(orders[0].id);
  expect(us.orders.map((order) => order.id)).not.toContain(orders[1].id);
  await rpc(context, "plan", {
    market: "us",
    action: "pause",
    version: us.plan.version,
  });
  expect(
    (
      await raw(context, "plan", {
        market: "us",
        action: "cancel",
        version: us.plan.version,
      })
    ).status(),
  ).toBe(409);
  const unchanged = await rpc<Account>(
    context,
    "account",
    { market: "uk" },
    true,
  );
  expect(unchanged.plan).toEqual(uk.plan);
  expect(
    (await rpc<Order>(context, "order", { id: orders[0].id }, true)).snapshot,
  ).toEqual(orders[0].snapshot);
  await visit(page, "/m/uk/en/account");
  await expect(
    page.getByRole("heading", { name: /Next box to review/ }),
  ).toBeVisible();
  await chooseOption(page.getByLabel("Delivery country"), "United States");
  await expect(
    page.getByRole("button", { name: "Resume plan", exact: true }),
  ).toBeVisible();
});

test("privacy export is owned, durable and separate from subscription cancellation", async ({
  page,
  context,
  browser,
}) => {
  await signIn(page, "sam@gather.example");
  const before = await rpc<Account>(context, "account", { market: "uk" }, true);
  const requestKey = crypto.randomUUID();
  const request = await rpc<Account["privacyRequests"][number]>(
    context,
    "requestPrivacy",
    { kind: "export", requestKey },
  );
  expect(
    (
      await rpc<{ id: string }>(context, "requestPrivacy", {
        kind: "export",
        requestKey,
      })
    ).id,
  ).toBe(request.id);
  expect(
    (
      await raw(context, "requestPrivacy", { kind: "deletion", requestKey })
    ).status(),
  ).toBe(409);
  const other = await browser.newContext({ baseURL: testOrigin });
  const otherPage = await other.newPage();
  await signIn(otherPage);
  expect((await raw(other, "privacyExport", { id: request.id })).status()).toBe(
    404,
  );
  expect((await raw(other, "cancelPrivacy", { id: request.id })).status()).toBe(
    404,
  );
  await other.close();
  const exported = await rpc<{ content: string }>(context, "privacyExport", {
    id: request.id,
  });
  const data = JSON.parse(exported.content);
  expect(data.identity.email).toBe("sam@gather.example");
  expect(data.addresses.length).toBeGreaterThan(0);
  expect(data.boxes).toContainEqual(
    expect.objectContaining({
      market: "us",
      cart: expect.objectContaining({ market: "us" }),
    }),
  );
  expect(data.orders.length).toBeGreaterThan(0);
  expect(
    data.orders.every(
      (order: { owner_id: string }) => order.owner_id === data.identity.id,
    ),
  ).toBe(true);
  expect(
    (
      await rpc<{ content: string }>(context, "privacyExport", {
        id: request.id,
      })
    ).content,
  ).toBe(exported.content);
  await visit(page, "/m/us/en/account/privacy");
  await page
    .getByRole("button", { name: "Request deletion", exact: true })
    .click();
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Submit request", exact: true })
    .click();
  await expect(page.getByRole("dialog")).not.toBeVisible();
  const after = await rpc<Account>(context, "account", { market: "uk" }, true);
  expect(after.plan).toEqual(before.plan);
  const deletion = after.privacyRequests.find(
    (entry) => entry.kind === "deletion" && entry.status === "requested",
  )!;
  expect(deletion).toBeTruthy();
  expect(
    (
      await rpc<{ status: string }>(context, "cancelPrivacy", {
        id: deletion.id,
      })
    ).status,
  ).toBe("canceled");
  expect(
    (await raw(context, "privacyExport", { id: deletion.id })).status(),
  ).toBe(404);
  await page.setViewportSize({ width: 390, height: 844 });
  await chooseOption(page.getByLabel("Language"), /中文/);
  await expect(page.getByRole("heading", { name: "隐私与数据" })).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
});
