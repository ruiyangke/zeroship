import { test, expect } from "./fixtures";
import { signIn, rpc, raw, visit } from "./helpers";
import { testOrigin, backofficeOrigin } from "../fixture/settings";
import { defaultCart, type Order, type Quote } from "@gather/meal-kit/domain";
import { deliveryDates } from "@gather/meal-kit/catalog";

test("concurrent privacy exports retain each customer's identity inside database callbacks", async ({ page, context, browser }) => {
  const other = await browser.newContext({ baseURL: testOrigin });
  try {
    await signIn(page);
    await signIn(await other.newPage(), "sam@gather.example");
    const documents = await Promise.all([context, other].map(async client => {
      const request = await rpc<{ id: string }>(client, "requestPrivacy", { kind: "export", requestKey: crypto.randomUUID() });
      const exported = await rpc<{ content: string }>(client, "privacyExport", { id: request.id });
      return JSON.parse(exported.content);
    }));
    expect(documents.map(document => document.identity.email)).toEqual(["alex@gather.example", "sam@gather.example"]);
    expect(documents[0].identity.id).not.toBe(documents[1].identity.id);
    for (const document of documents) expect(document.orders.every((order: { owner_id: string }) => order.owner_id === document.identity.id)).toBe(true);
  } finally { await other.close(); }
});

test("a box the storefront writes is on the back office board, through the shared database", async ({ page, context, browser }) => {
  const ops = await browser.newContext({ baseURL: backofficeOrigin });
  try {
    await signIn(page);
    const opsPage = await ops.newPage();
    await signIn(opsPage, "ops@gather.example");
    const customer = await rpc<{ user: { id: string }; staff: unknown }>(context, "session", {}, true);
    const operator = await rpc<{ user: { id: string } }>(ops, "session", {}, true);
    expect(customer.staff).toBeNull();
    expect(customer.user.id).not.toBe(operator.user.id);
    const cart = { ...defaultCart("us"), deliveryDate: deliveryDates("us").at(-1)!, postal: "10001", recurring: false, mealCount: 2 as const, recipeIds: ["lemon-chicken", "pesto-pasta"] };
    const order = await rpc<Order>(context, "checkout", {
      cart, quote: await rpc<Quote>(context, "quote", cart), requestKey: crypto.randomUUID(), consent: true,
      address: { country: "US", province: "NY", district: "", name: "Cross app customer", email: "alex@gather.example", line: "12 Garden Street", city: "New York", postal: cart.postal, phone: "+12125550123", instructions: "" },
    });
    const board = await rpc<{ orders: Order[] }>(ops, "operations", { market: "us" }, true);
    expect(board.orders.find(row => row.id === order.id)).toMatchObject({ market: "us", total: order.total, payment: "succeeded" });
    await rpc(ops, "advance", { id: order.id, next: "packing" });
    expect(await rpc(context, "order", { id: order.id }, true)).toMatchObject({ fulfillment: "packing" });
    await visit(opsPage, "/m/us/en/operations");
    await expect(opsPage.getByText("Cross app customer", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: "My account", exact: true }).click();
    await expect(page.getByRole("menuitem", { name: "Operations", exact: true })).toHaveCount(0);
    await expect(page.getByRole("menuitem", { name: "My deliveries", exact: true })).toBeVisible();
    expect((await raw(context, "operations", { market: "us" }, true)).status()).toBe(404);
    const staffRead = await context.request.post(backofficeOrigin + "/__zeroship/v1/gather.operations", { data: { json: { market: "us" } }, headers: { "X-Method": "GET" } });
    expect(staffRead.status()).toBe(401);
  } finally { await ops.close(); }
});

