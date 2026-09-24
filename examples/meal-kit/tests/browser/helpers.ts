import { expect, type Page, type BrowserContext } from "@playwright/test";
import { defaultCart, type Cart, type Order } from "@gather/meal-kit/domain";
import type { DraftLoad, DraftSave } from "@gather/meal-kit/draft-domain";
import { draftRevision } from "@gather/meal-kit/draft-domain";
import { testOrigin, backofficeOrigin } from "../fixture/settings";
const origins = new WeakMap<BrowserContext, string>();
export function visit(page: Page, path: string) {
  const origin = path.includes("/operations") ? backofficeOrigin : origins.get(page.context()) ?? testOrigin;
  return page.goto(new URL(path, origin).href);
}
export async function signIn(
  page: Page,
  email = "alex@gather.example",
  resetBox = true,
  surface: "storefront" | "backoffice" = email === "ops@gather.example" ? "backoffice" : "storefront",
) {
  origins.set(page.context(), surface === "backoffice" ? backofficeOrigin : testOrigin);
  await visit(page, "/m/us/en");
  const opened = page.waitForEvent("popup");
  await page.getByRole("button", { name: "Log in", exact: true }).click();
  const popup = await opened;
  await popup.getByLabel("Dev user").selectOption(email);
  await popup.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "My account", exact: true }),
  ).toBeVisible();
  if (resetBox && surface === "storefront") {
    await seedBox(page.context(), defaultCart("us"));
    await page.reload();
  }
}

export async function boxSession(
  context: BrowserContext,
  market: Cart["market"] = "us",
) {
  const session = await context.request.post("/api/draft-session", {
    headers: { "X-Gather-Session": "init" },
  });
  expect(session.ok(), await session.text()).toBe(true);
  const { csrf } = await session.json();
  const { user } = await rpc<{ user: { id: string } | null }>(
    context,
    "session",
    {},
    true,
  );
  return { market, csrf, owner: user?.id ?? null };
}
export async function loadBox(
  context: BrowserContext,
  market: Cart["market"] = "us",
) {
  const scope = await boxSession(context, market);
  const response = await context.request.post("/api/drafts/load", {
    data: scope,
  });
  expect(response.ok(), await response.text()).toBe(true);
  return { scope, state: (await response.json()) as DraftLoad };
}
export async function seedBox(context: BrowserContext, cart: Cart) {
  const { scope, state } = await loadBox(context, cart.market);
  const response = await context.request.post("/api/drafts/save", {
    data: {
      ...scope,
      cart,
      expected: draftRevision(state.draft),
      requestKey: crypto.randomUUID(),
    },
  });
  expect(response.ok(), await response.text()).toBe(true);
  const result = (await response.json()) as DraftSave;
  expect(result.saved).toBe(true);
  return result.state.draft!;
}
export async function raw(
  context: BrowserContext,
  name: string,
  input: unknown = {},
  query = false,
) {
  return context.request.post(`${origins.get(context) ?? testOrigin}/__zeroship/v1/gather.${name}`, {
    data: { json: input },
    headers: query ? { "X-Method": "GET" } : {},
  });
}
export async function rpc<T>(
  context: BrowserContext,
  name: string,
  input: unknown = {},
  query = false,
): Promise<T> {
  const response = await raw(context, name, input, query);
  expect(response.ok(), await response.text()).toBe(true);
  return (await response.json()).json as T;
}

export async function chooseOption(
  control: import("@playwright/test").Locator,
  label: string | RegExp,
) {
  await control.click();
  const list = control.page().getByRole("listbox");
  await expect(list).toHaveCount(1);
  await list
    .getByRole("option", { name: label, exact: typeof label === "string" })
    .click();
  await expect(list).toHaveCount(0);
}

export async function subject(context: BrowserContext) {
  return (await rpc<{ user: { id: string } }>(context, "session", {}, true)).user.id;
}

// `requirePaymentReady` refuses a new checkout while this customer has an
// unfinished attempt in this market, and an attempt outlives the spec that
// started one. A spec that checks out as a shared demo customer therefore
// states that the customer has none, instead of inheriting whether the spec
// before it reached its own settlement. Returns how many it had to settle, so
// a caller can say whether it inherited anything.
export async function paymentReady(
  context: BrowserContext,
  market: Cart["market"] = "us",
) {
  // The same pair `requirePaymentReady` blocks on: a live hold AND a payment
  // still in flight. An order whose hold ended is `checkout_expired` and
  // refuses nothing, so counting it here would name a leftover that blocks
  // nobody. `gather.account` sweeps expiry for this market before it answers,
  // so `pending_payment` is the live hold as of this read.
  const unfinished = (o: Order) =>
    o.status === "pending_payment" &&
    ["processing", "requires_action"].includes(o.payment);
  const inherited = (
    await rpc<{ orders: Order[] }>(context, "account", { market }, true)
  ).orders.filter(unfinished);
  for (const order of inherited) {
    // Only a verification an earlier spec armed and left can be settled from
    // here. Anything else is named rather than skipped past, because skipping
    // it would put the PAYMENT_PENDING refusal back on the next checkout.
    expect(
      order.payment,
      `order ${order.id} (${order.status}) was left unfinished by an earlier spec and cannot be settled here`,
    ).toBe("requires_action");
    await rpc(context, "pay", { id: order.id });
  }
  expect(
    (
      await rpc<{ orders: Order[] }>(context, "account", { market }, true)
    ).orders.filter(unfinished),
  ).toEqual([]);
  return inherited.length;
}
