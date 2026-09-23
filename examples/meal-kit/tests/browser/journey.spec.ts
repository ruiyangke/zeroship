import { subject } from "./helpers";
import { visit } from "./helpers";
import { chooseOption } from "./helpers";
import { testOrigin } from "../fixture/settings";
import { recipes } from "../../apps/backoffice/src/seed-catalog";
import { test, expect } from "./fixtures";
import type { Page, BrowserContext } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";
import { markets, deliveryDates } from "@gather/meal-kit/catalog";
import {
  defaultCart,
  type Cart,
  type Order,
  type Quote,
} from "@gather/meal-kit/domain";

import { signIn, rpc, raw, loadBox } from "./helpers";
import type { getAccount } from "../fixture/api";
const newCart = (): Cart => ({
  ...defaultCart(),
  postal: markets.us.postal,
  deliveryDate: deliveryDates("us")[0],
  recipeIds: recipes.slice(0, 3).map((r) => r.id),
});

test("storefront, checkout, recovery, ownership, support and fulfillment", async ({
  page,
  context,
  browser,
}) => {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  const ops = await browser.newContext({ baseURL: testOrigin });
  const opsPage = await ops.newPage();
  await signIn(opsPage, "ops@gather.example");
  await signIn(page);
  await rpc(ops, "paymentScenario", {
    customerId: await subject(context),
    market: "us",
    outcome: "requires_action",
  });
  await visit(page, "/m/us/en/plans");
  await page.getByLabel("ZIP code").fill("10001");
  await page
    .getByRole("button", { name: "Continue to box size", exact: true })
    .click();
  await page
    .getByRole("button", { name: "See available meals", exact: true })
    .click();
  for (const name of [
    "Lemon & herb chicken",
    "Garden pesto rigatoni",
    "Miso-glazed salmon",
  ])
    await page
      .getByRole("button", { name: `Add ${name}`, exact: true })
      .click();
  await page
    .getByRole("link", { name: "Review your box", exact: true })
    .click();
  await page.getByRole("link", { name: "Continue to checkout" }).click();
  await page.getByLabel("Street address").fill("123 Garden Street");
  await page.getByLabel("Phone number").fill("+12125550123");
  expect(
    (
      await raw(context, "paymentScenario", {
        customerId: await subject(context),
        market: "us",
        outcome: "succeeded",
      })
    ).status(),
  ).toBe(404);
  await expect(page.getByLabel("Payment scenario")).toHaveCount(0);
  await page.getByRole("checkbox").check();
  await page.getByRole("button", { name: "Place order", exact: true }).click();
  await expect(page).toHaveURL(/\/orders\//);
  await expect
    .poll(async () => (await loadBox(context)).state.draft?.cart.recipeIds)
    .toEqual([]);
  const id = page.url().split("/").at(-1)!;
  await expect(
    page.getByRole("button", { name: "Complete verification" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Complete verification" }).click();
  await expect(
    page.getByText("Your order is confirmed.", { exact: true }),
  ).toBeVisible();
  await page.reload();
  await expect(
    page.getByText("123 Garden Street", { exact: false }),
  ).toBeVisible();
  const paid = await rpc<Order>(context, "order", { id }, true);
  expect(paid.payment).toBe("succeeded");
  const anonymous = await browser.newContext({
    baseURL: testOrigin,
  });
  expect((await raw(anonymous, "order", { id }, true)).status()).toBe(401);
  await anonymous.close();
  const other = await browser.newContext({ baseURL: testOrigin });
  const otherPage = await other.newPage();
  await signIn(otherPage, "sam@gather.example");
  expect((await raw(other, "order", { id }, true)).status()).toBe(404);
  expect((await raw(other, "cancelOrder", { id })).status()).toBe(404);
  expect(
    (await raw(other, "operations", { market: "us" }, true)).status(),
  ).toBe(404);
  expect((await raw(other, "receipt", { id })).status()).toBe(404);
  await other.close();
  await page.getByRole("button", { name: "Report an issue" }).click();
  await page
    .getByLabel("Details", { exact: true })
    .fill("A lemon is missing from my ingredient bag.");
  const submitted = page.waitForResponse((response) =>
    new URL(response.url()).pathname.endsWith("/gather.issue"),
  );
  await page.getByRole("button", { name: "Send request", exact: true }).click();
  const response = await submitted;
  expect(response.ok(), await response.text()).toBe(true);
  await expect(page.getByRole("dialog")).not.toBeVisible();

  const work = await rpc<{ cases: { id: string; order_id: string }[] }>(
    ops,
    "operations",
    { market: "us" },
    true,
  );
  const issue = work.cases.find((c) => c.order_id === id)!;
  expect(issue).toBeTruthy();
  expect(
    (
      await raw(ops, "resolve", {
        id: issue.id,
        resolution: "Refund the missing ingredient.",
        refund: paid.total + 1,
        requestKey: crypto.randomUUID(),
      })
    ).status(),
  ).toBe(409);
  await rpc(ops, "resolve", {
    id: issue.id,
    resolution: "We have refunded the missing ingredient.",
    refund: 100,
    requestKey: crypto.randomUUID(),
  });
  for (const next of [
    "packing",
    "packed",
    "dispatched",
    "exception",
    "dispatched",
    "delivered",
  ])
    await rpc(ops, "advance", { id, next });
  expect((await raw(context, "cancelOrder", { id })).status()).toBe(409);
  await page.reload();
  await expect(
    page.getByRole("heading", { name: "Delivered", exact: true }),
  ).toBeVisible();
  const download = page.waitForEvent("download");
  await page.getByRole("button", { name: "Download order details" }).click();
  expect((await download).suggestedFilename()).toContain(id);
  const cart = newCart();
  const quote = await rpc<Quote>(context, "quote", cart);
  const requestKey = crypto.randomUUID();
  const input = {
    cart,
    address: paid.snapshot.address,
    quote,
    requestKey,
    outcome: "succeeded",
    consent: true,
  };
  const created = await rpc<Order>(context, "checkout", input);
  expect((await rpc<Order>(context, "checkout", input)).id).toBe(created.id);
  expect(
    (
      await raw(context, "checkout", {
        ...input,
        requestKey: crypto.randomUUID(),
        quote: { ...quote, total: 1 },
      })
    ).status(),
  ).toBe(409);
  const canceled = await rpc<Order>(context, "cancelOrder", { id: created.id });
  expect(canceled.refunded).toBe(created.total);
  expect(
    (await rpc<Order>(context, "cancelOrder", { id: created.id })).refunded,
  ).toBe(created.total);
  async function changePlan(action: string) {
    const account = await rpc<Awaited<ReturnType<typeof getAccount>>>(
      context,
      "account",
      { market: "us" },
      true,
    );
    return rpc(context, "plan", {
      market: "us",
      version: account.plan.version,
      action,
    });
  }
  await changePlan("pause");
  expect(
    (await raw(context, "renewalPreview", { market: "us" }, true)).status(),
  ).toBe(409);
  await changePlan("resume");
  const renewal = await rpc<{ date: string; quote: Quote }>(
    context,
    "renewalPreview",
    { market: "us" },
    true,
  );
  const renewed = await rpc<Order>(context, "renewal", {
    market: "us",
    date: renewal.date,
    total: renewal.quote.total,
  });
  expect(
    (
      await rpc<Order>(context, "renewal", {
        market: "us",
        date: renewal.date,
        total: renewal.quote.total,
      })
    ).id,
  ).toBe(renewed.id);
  await changePlan("skip");
  await changePlan("cancel");
  expect(
    (await raw(context, "renewalPreview", { market: "us" }, true)).status(),
  ).toBe(409);
  await visit(opsPage, "/m/us/en/operations");
  await expect(
    opsPage.getByRole("heading", {
      name: "Good food, thoughtfully delivered.",
    }),
  ).toBeVisible();
  await opsPage
    .getByRole("tab", { name: "Inventory & menu", exact: true })
    .click();
  await opsPage
    .getByRole("button", { name: "Prepare menu inventory", exact: true })
    .click();
  await expect(
    opsPage
      .getByRole("cell", { name: "Sticky ginger tofu", exact: true })
      .first(),
  ).toBeVisible();
  const stockKey = `us:${cart.deliveryDate}:sesame-tofu`;
  await rpc(ops, "inventory", { stockKey, available: 0, published: false });
  const blockedCart = {
    ...cart,
    recipeIds: ["lemon-chicken", "pesto-pasta", "sesame-tofu"],
  };
  const blockedQuote = await rpc<Quote>(context, "quote", blockedCart);
  expect(
    (
      await raw(context, "checkout", {
        ...input,
        cart: blockedCart,
        quote: blockedQuote,
        requestKey: crypto.randomUUID(),
      })
    ).status(),
  ).toBe(409);
  await rpc(ops, "inventory", { stockKey, available: 40, published: true });
  await ops.close();
  expect(errors).toEqual([]);
});

test("global navigation, mobile layout and accessible controls", async ({
  page,
}) => {
  await visit(page, "/m/us/en");
  await expect(
    page.getByRole("heading", { name: "Make room for good food." }),
  ).toBeVisible();
  const results = await new AxeBuilder({ page })
    .withTags(["wcag2a", "wcag2aa", "wcag21aa"])
    .analyze();
  expect(results.violations).toEqual([]);
  await chooseOption(page.getByLabel("Delivery country"), "China");
  await chooseOption(page.getByLabel("Language", { exact: true }), /中文/);
  await expect(page).toHaveURL(/\/m\/cn\/zh\/plans/);
  await chooseOption(page.getByLabel("省份"), "上海");
  await chooseOption(page.getByLabel("城市", { exact: true }), "上海");
  await chooseOption(page.getByLabel("区／县"), "浦东新区");
  await page
    .getByRole("button", { name: "继续选择餐盒规格", exact: true })
    .click();
  await page.getByRole("button", { name: "查看可选菜品", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "这周想吃点什么？" }),
  ).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: "tests/.artifacts/menu-mobile-zh.png",
    fullPage: true,
  });
});

test("language switching preserves checkout input and supports localized search", async ({
  page,
}) => {
  await signIn(page);
  await visit(page, "/m/us/en/plans");
  await page.getByLabel("ZIP code").fill("10001");
  await page
    .getByRole("button", { name: "Continue to box size", exact: true })
    .click();
  await page
    .getByRole("button", { name: "See available meals", exact: true })
    .click();
  for (const name of ["Lemon & herb chicken", "Garden pesto rigatoni"]) {
    await page
      .getByRole("button", { name: `Add ${name}`, exact: true })
      .click();
  }
  await expect(
    page.getByText("Choose 1 more meal", { exact: true }),
  ).toBeVisible();
  await chooseOption(page.getByLabel("Language", { exact: true }), /中文/);
  await expect(page.locator("html")).toHaveAttribute("lang", "zh-CN");
  await expect(page.getByText("再选 1 道菜", { exact: true })).toBeVisible();
  await page.getByLabel("搜索菜品").fill("三文鱼");
  await expect(page.locator(".meal-card")).toHaveCount(1);
  await page
    .getByRole("button", { name: "添加味噌照烧三文鱼", exact: true })
    .click();
  await chooseOption(page.getByLabel("语言", { exact: true }), "English");
  await page
    .getByRole("link", { name: "Review your box", exact: true })
    .click();
  await page
    .getByRole("link", { name: "Continue to checkout", exact: true })
    .click();
  await page.getByLabel("Street address").fill("123 Garden Street");
  await page.getByLabel("Phone number").fill("+12125550123");
  await chooseOption(page.getByLabel("Language", { exact: true }), /中文/);
  await expect(page).toHaveURL(/\/m\/us\/zh\/checkout/);
  await expect(page.getByLabel("详细地址")).toHaveValue("123 Garden Street");
  await expect(page.getByLabel("电话号码")).toHaveValue("+12125550123");
  await expect(page.getByRole("combobox", { name: "配送国家" })).toContainText(
    "美国",
  );
  await expect(page.locator(".box-summary")).toContainText("US$");
  await chooseOption(page.getByLabel("语言", { exact: true }), "English");
  await expect(page.getByLabel("Street address")).toHaveValue(
    "123 Garden Street",
  );
  await visit(page, "/m/cn/zh/recipes/lemon-chicken");
  await expect(
    page.getByRole("heading", { name: "柠檬香草烤鸡", exact: true }),
  ).toBeVisible();
  await expect(page).toHaveTitle("Gather — 让好好吃饭更简单");
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("lang", "zh-CN");
  await visit(page, "/m/cn/unsupported/menu?view=saved#recipes");
  await expect(page).toHaveURL(/\/m\/cn\/en\/menu\?view=saved#recipes$/);
  await expect(
    page.getByRole("combobox", { name: "Language", exact: true }),
  ).toContainText("English");
});

test("catalog download failure keeps the interface available and can recover", async ({
  page,
}) => {
  await page.route("**/locales/zh/messages.po*", (route) => route.abort());
  await visit(page, "/m/us/en/menu");
  await chooseOption(page.getByLabel("Language", { exact: true }), /中文/);
  await expect(page.getByRole("alert")).toContainText(
    "We couldn't load this language.",
  );
  await expect(
    page.getByRole("combobox", { name: "Language", exact: true }),
  ).toContainText("English");
  await page.unroute("**/locales/zh/messages.po*");
  await page
    .getByRole("button", { name: "Reload and retry", exact: true })
    .click();
  await expect(page.locator("html")).toHaveAttribute("lang", "zh-CN");
  await expect(page.getByRole("alert")).not.toBeVisible();
  await expect(
    page.getByRole("heading", { name: "这周想吃点什么？" }),
  ).toBeVisible();
});
