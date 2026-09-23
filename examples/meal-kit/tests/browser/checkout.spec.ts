import { subject } from "./helpers";
import { visit } from "./helpers";
import { chooseOption } from "./helpers";
import { test, expect } from "./fixtures";
import { testOrigin } from "../fixture/settings";
import { signIn, rpc, raw } from "./helpers";
import { defaultCart, type Order, type Quote } from "@gather/meal-kit/domain";
import { deliveryDates } from "@gather/meal-kit/catalog";
import type * as api from "../fixture/api";
import type { MenuDraft } from "@gather/meal-kit/catalog-domain";
import { money } from "@gather/meal-kit/catalog";
import AxeBuilder from "@axe-core/playwright";

test("delayed checkout holds the last slot, expiry releases it, and late payment cannot release fulfillment", async ({
  page,
  context,
  browser,
}) => {
  const ops = await browser.newContext({ baseURL: testOrigin });
  const other = await browser.newContext({ baseURL: testOrigin });
  try {
    const opsPage = await ops.newPage();
    const otherPage = await other.newPage();
    await signIn(opsPage, "ops@gather.example");
    await signIn(page);
    await signIn(otherPage, "sam@gather.example");
    const market = "uk";
    const date = deliveryDates(market).at(-1)!;
    const catalog = () =>
      rpc<Awaited<ReturnType<typeof api.getCatalog>>>(
        context,
        "catalog",
        { market, date },
        true,
      );
    const menu = (await catalog()).menu!;
    expect(menu.recipes.length).toBeGreaterThanOrEqual(2);
    const cart = {
      ...defaultCart(market),
      deliveryDate: date,
      postal: "SW1A 1AA",
      mealCount: 2 as const,
      recurring: false,
      recipeIds: menu.recipes.slice(0, 2).map((r) => r.id),
    };
    const address = {
      country: "GB",
      province: "",
      district: "",
      name: "Alex Morgan",
      email: "alex@gather.example",
      line: "20 Garden Street",
      city: "London",
      postal: cart.postal,
      phone: "+442079460000",
      instructions: "",
    };
    for (const id of [...cart.recipeIds, "delivery"])
      await rpc(ops, "inventory", {
        stockKey: `${market}:${date}:${id}`,
        available: id === "delivery" ? 1 : cart.servings,
        published: true,
      });
    for (const customerId of [
      await subject(context),
      await subject(other),
    ])
      await rpc(ops, "paymentScenario", {
        customerId,
        market,
        outcome: "processing",
      });
    const contexts = [context, other];
    const inputs = await Promise.all(
      contexts.map(async (ctx) => ({
        cart,
        address,
        quote: await rpc<Quote>(ctx, "quote", cart),
        requestKey: crypto.randomUUID(),
        consent: true,
      })),
    );
    const responses = await Promise.all(
      contexts.map((ctx, index) => raw(ctx, "checkout", inputs[index])),
    );
    expect(responses.filter((r) => r.ok())).toHaveLength(1);
    const winnerIndex = responses.findIndex((r) => r.ok());
    const loserIndex = 1 - winnerIndex;
    expect(
      responses[loserIndex].status(),
      await responses[loserIndex].text(),
    ).toBe(409);
    const held = (await responses[winnerIndex].json()).json as Order;
    expect(held).toMatchObject({
      status: "pending_payment",
      payment: "processing",
    });
    expect(Date.parse(held.snapshot.paymentDeadline!)).toBeGreaterThan(
      Date.now(),
    );
    const attemptId = held.snapshot.paymentAttemptId!;
    const winner = contexts[winnerIndex];
    const loser = contexts[loserIndex];
    const winnerPage = winnerIndex === 0 ? page : otherPage;
    const duplicate = await rpc<Order>(winner, "checkout", inputs[winnerIndex]);
    expect(duplicate.id).toBe(held.id);
    expect(
      (
        await raw(winner, "checkout", {
          ...inputs[winnerIndex],
          requestKey: crypto.randomUUID(),
        })
      ).status(),
    ).toBe(409);
    expect((await catalog()).availability.every((r) => r.available === 0)).toBe(
      true,
    );
    expect((await raw(winner, "pay", { id: held.id })).status()).toBe(409);
    expect((await raw(loser, "pay", { id: held.id })).status()).toBe(404);
    for (const name of ["expireDemoCheckout", "settleDemoPayment"])
      expect(
        (await raw(winner, name, { attemptId, outcome: "succeeded" })).status(),
      ).toBe(404);
    expect(
      (await raw(ops, "advance", { id: held.id, next: "packing" })).status(),
    ).toBe(409);
    await visit(winnerPage, `/m/uk/en/orders/${held.id}`);
    await expect(
      winnerPage.getByText(/We're waiting for your payment confirmation/),
    ).toBeVisible();
    await expect(
      winnerPage.getByRole("button", { name: "Try payment again" }),
    ).toHaveCount(0);
    await winnerPage.reload();
    await expect(
      winnerPage.getByRole("button", { name: "Check payment status" }),
    ).toBeVisible();
    await visit(opsPage, "/m/uk/en/operations");
    const row = opsPage.getByRole("row").filter({ hasText: held.id });
    await row.getByRole("button", { name: "Simulate checkout expiry" }).click();
    await expect(
      row.getByRole("button", { name: "Simulate checkout expiry" }),
    ).toHaveCount(0);
    const expired = await rpc<Order>(winner, "order", { id: held.id }, true);
    expect(expired.status).toBe("checkout_expired");
    const released = await catalog();
    expect(
      released.availability
        .filter((r) => cart.recipeIds.includes(r.recipeId))
        .every((r) => r.available === cart.servings),
    ).toBe(true);
    await rpc(ops, "expireDemoCheckout", { attemptId });
    await rpc(ops, "sweepCheckouts", { market });
    expect((await catalog()).availability).toEqual(released.availability);
    await rpc(ops, "paymentScenario", {
      customerId:
        loserIndex === 0
          ? await subject(context)
          : await subject(other),
      market,
      outcome: "succeeded",
    });
    const replacement = await rpc<Order>(loser, "checkout", inputs[loserIndex]);
    expect(replacement.status).toBe("confirmed");
    await row
      .getByRole("button", { name: "Simulate payment confirmation" })
      .click();
    await expect(
      row.getByRole("button", { name: "Refund late payment" }),
    ).toBeVisible();
    const late = await rpc<Order>(winner, "order", { id: held.id }, true);
    expect(late).toMatchObject({
      payment: "succeeded",
      status: "payment_recovery",
      fulfillment: "unallocated",
      refunded: 0,
    });
    expect(
      (
        await rpc<Order>(ops, "settleDemoPayment", {
          attemptId,
          outcome: "succeeded",
        })
      ).version,
    ).toBe(late.version);
    expect(
      (await raw(ops, "advance", { id: late.id, next: "packing" })).status(),
    ).toBe(409);
    expect(
      (
        await raw(winner, "editOrder", {
          id: late.id,
          recipeIds: cart.recipeIds,
          quote: inputs[winnerIndex].quote,
        })
      ).status(),
    ).toBe(400);
    await winnerPage.reload();
    await expect(
      winnerPage.getByText(
        /Your payment arrived after your meals were released/,
      ),
    ).toBeVisible();
    await expect(
      winnerPage
        .locator(".timeline")
        .getByText("Payment under review", { exact: true }),
    ).toBeVisible();
    expect(
      (await new AxeBuilder({ page: winnerPage }).analyze()).violations,
    ).toEqual([]);
    await winnerPage.setViewportSize({ width: 390, height: 844 });
    await expect
      .poll(() =>
        winnerPage.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      )
      .toBe(true);
    await winnerPage.screenshot({
      path: "tests/.artifacts/payment-review-mobile.png",
      fullPage: true,
    });
    await chooseOption(
      winnerPage.getByLabel("Language", { exact: true }),
      /中文/,
    );
    await expect(
      winnerPage.locator(".timeline").getByText("支付待核实", { exact: true }),
    ).toBeVisible();
    await winnerPage.screenshot({
      path: "tests/.artifacts/payment-review-zh-mobile.png",
      fullPage: true,
    });
    expect(
      (await raw(winner, "refundRecoveredPayment", { id: late.id })).status(),
    ).toBe(404);
    await row.getByRole("button", { name: "Refund late payment" }).click();
    const refunded = await rpc<Order>(winner, "order", { id: late.id }, true);
    expect(refunded).toMatchObject({
      status: "canceled",
      refunded: held.total,
    });
    expect(
      (await rpc<Order>(ops, "refundRecoveredPayment", { id: late.id }))
        .version,
    ).toBe(refunded.version);
    expect((await catalog()).availability.every((r) => r.available === 0)).toBe(
      true,
    );
    expect(
      (
        await raw(ops, "settleDemoPayment", { attemptId, outcome: "failed" })
      ).status(),
    ).toBe(409);
    await rpc(loser, "cancelOrder", { id: replacement.id });
    expect((await catalog()).availability).toEqual(released.availability);
  } finally {
    await ops.close();
    await other.close();
  }
});

test("verification keeps its reserved price and declined payments require a newly approved total", async ({
  page,
  context,
  browser,
}) => {
  const ops = await browser.newContext({ baseURL: testOrigin });
  try {
    const opsPage = await ops.newPage();
    await signIn(opsPage, "ops@gather.example");
    await signIn(page);
    const market = "uk";
    const date = deliveryDates(market).at(-2)!;
    const catalog = () =>
      rpc<Awaited<ReturnType<typeof api.getCatalog>>>(
        context,
        "catalog",
        { market, date },
        true,
      );
    const initial = await catalog();
    const cart = {
      ...defaultCart(market),
      deliveryDate: date,
      postal: "SW1A 1AA",
      recurring: false,
      recipeIds: initial.recipes.slice(0, 3).map((r) => r.id),
    };
    const address = {
      country: "GB",
      province: "",
      district: "",
      name: "Alex Morgan",
      email: "alex@gather.example",
      line: "20 Garden Street",
      city: "London",
      postal: cart.postal,
      phone: "+442079460000",
      instructions: "",
    };
    const scenario = async (outcome: string) =>
      rpc(ops, "paymentScenario", {
        customerId: await subject(context),
        market,
        outcome,
      });
    const increasePrice = async () => {
      const workspace = await rpc<
        Awaited<ReturnType<typeof api.getCatalogWorkspace>>
      >(ops, "catalogWorkspace", { market }, true);
      const menu = workspace.menus.find((row) => row.delivery_date === date)!;
      const draft = menu.draft as MenuDraft;
      const saved = await rpc<typeof menu>(ops, "saveMenuDraft", {
        market,
        date,
        id: menu.id,
        version: menu.version,
        draft: { ...draft, price: draft.price + 100 },
      });
      await rpc(ops, "publishMenu", { id: saved.id, version: saved.version });
    };
    await scenario("requires_action");
    const input = {
      cart,
      address,
      quote: await rpc<Quote>(context, "quote", cart),
      requestKey: crypto.randomUUID(),
      consent: true,
    };
    for (const change of [
      { currency: "USD" },
      { subtotal: input.quote.subtotal + 1 },
      {
        expiresAt: new Date(
          Date.parse(input.quote.expiresAt) + 60_000,
        ).toISOString(),
      },
    ])
      expect(
        (
          await raw(context, "checkout", {
            ...input,
            quote: { ...input.quote, ...change },
          })
        ).status(),
      ).toBe(409);
    const held = await rpc<Order>(context, "checkout", input);
    expect(held.payment).toBe("requires_action");
    await increasePrice();
    await visit(page, `/m/uk/en/orders/${held.id}`);
    await expect(page.getByText(/Complete verification by/)).toBeVisible();
    await page
      .getByRole("button", { name: "Complete verification", exact: true })
      .click();
    await expect(
      page.getByRole("heading", { name: "Your order is confirmed." }),
    ).toBeVisible();
    const paid = await rpc<Order>(context, "order", { id: held.id }, true);
    expect(paid.total).toBe(input.quote.total);
    expect((await rpc<Order>(context, "pay", { id: held.id })).version).toBe(
      paid.version,
    );
    expect((await rpc<Order>(context, "checkout", input)).id).toBe(held.id);
    await rpc(context, "cancelOrder", { id: held.id });
    expect((await catalog()).availability).toEqual(initial.availability);
    await scenario("failed");
    const declinedInput = {
      ...input,
      quote: await rpc<Quote>(context, "quote", cart),
      requestKey: crypto.randomUUID(),
    };
    const declined = await rpc<Order>(context, "checkout", declinedInput);
    expect(declined.payment).toBe("failed");
    expect((await catalog()).availability).toEqual(initial.availability);
    expect((await raw(context, "pay", { id: declined.id })).status()).toBe(409);
    await increasePrice();
    expect(
      (
        await raw(context, "pay", {
          id: declined.id,
          quote: declinedInput.quote,
        })
      ).status(),
    ).toBe(409);
    const expectedQuote = await rpc<Quote>(context, "quote", cart);
    await scenario("processing");
    const inProgress = await rpc<Order>(context, "checkout", {
      ...input,
      quote: expectedQuote,
      requestKey: crypto.randomUUID(),
    });
    expect(
      (
        await raw(context, "pay", { id: declined.id, quote: expectedQuote })
      ).status(),
    ).toBe(409);
    await rpc(context, "cancelOrder", { id: inProgress.id });
    await visit(page, `/m/uk/en/orders/${declined.id}`);
    await page
      .getByRole("button", { name: "Try payment again", exact: true })
      .click();
    const dialog = page.getByRole("dialog");
    await expect(
      dialog.getByText(
        `Total due: ${money(expectedQuote.total, market, "en")}`,
      ),
    ).toBeVisible();
    await dialog
      .getByRole("button", { name: "Confirm payment", exact: true })
      .click();
    await expect(dialog).not.toBeVisible();
    const retried = await rpc<Order>(
      context,
      "order",
      { id: declined.id },
      true,
    );
    expect(retried).toMatchObject({
      status: "confirmed",
      payment: "succeeded",
      total: expectedQuote.total,
    });
    expect(retried.snapshot.paymentAttemptId).not.toBe(
      declined.snapshot.paymentAttemptId,
    );
    expect(
      (
        await raw(ops, "settleDemoPayment", {
          attemptId: declined.snapshot.paymentAttemptId,
          outcome: "succeeded",
        })
      ).status(),
    ).toBe(409);
    await rpc(context, "cancelOrder", { id: retried.id });
    await rpc(context, "cancelOrder", { id: retried.id });
    expect((await catalog()).availability).toEqual(initial.availability);
    await scenario("requires_action");
    const awaiting = await rpc<Order>(context, "checkout", {
      ...input,
      quote: await rpc<Quote>(context, "quote", cart),
      requestKey: crypto.randomUUID(),
    });
    const canceled = await rpc<Order>(context, "cancelOrder", {
      id: awaiting.id,
    });
    expect(canceled.status).toBe("canceled");
    expect(
      (await rpc<Order>(context, "cancelOrder", { id: awaiting.id })).version,
    ).toBe(canceled.version);
    expect((await catalog()).availability).toEqual(initial.availability);
    const afterCancel = await rpc<Order>(ops, "settleDemoPayment", {
      attemptId: awaiting.snapshot.paymentAttemptId,
      outcome: "succeeded",
    });
    expect(afterCancel.status).toBe("payment_recovery");
    await rpc(ops, "refundRecoveredPayment", { id: awaiting.id });
    expect((await catalog()).availability).toEqual(initial.availability);
  } finally {
    await ops.close();
  }
});
