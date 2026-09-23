import { visit } from "./helpers";
import AxeBuilder from "@axe-core/playwright";
import { test, expect } from "./fixtures";
import { testOrigin } from "../fixture/settings";
import { chooseOption, signIn, rpc, raw } from "./helpers";
import { defaultCart, type Order, type Quote } from "@gather/meal-kit/domain";
import { deliveryDates } from "@gather/meal-kit/catalog";
import type { CookingRecipe, RecipeFeedback } from "@gather/meal-kit/cooking-domain";

test("purchased cooking preserves quantities and feedback is owned, delivered and revision checked", async ({
  page,
  context,
  browser,
}) => {
  const ops = await browser.newContext({ baseURL: testOrigin });
  const other = await browser.newContext({ baseURL: testOrigin });
  const guest = await browser.newContext({ baseURL: testOrigin });
  try {
    const opsPage = await ops.newPage();
    await signIn(opsPage, "ops@gather.example");
    await signIn(page);
    await signIn(await other.newPage(), "sam@gather.example");
    const cart = {
      ...defaultCart("us"),
      deliveryDate: deliveryDates("us").at(-1)!,
      postal: "10001",
      recurring: false,
      mealCount: 2 as const,
      recipeIds: ["lemon-chicken", "pesto-pasta"],
    };
    const order = await rpc<Order>(context, "checkout", {
      cart,
      quote: await rpc<Quote>(context, "quote", cart),
      requestKey: crypto.randomUUID(),
      consent: true,
      address: {
        country: "US",
        province: "NY",
        district: "",
        name: "Alex Morgan",
        email: "alex@gather.example",
        line: "12 Garden Street",
        city: "New York",
        postal: cart.postal,
        phone: "+12125550123",
        instructions: "",
      },
    });
    const recipe = order.snapshot.recipes.find(
      (recipe) => recipe.id === "lemon-chicken",
    )!;
    const identity = { orderId: order.id, recipeId: recipe.id };
    const review = {
      ...identity,
      expectedVersion: null,
      requestKey: crypto.randomUUID(),
      rating: 4,
      cookAgain: true,
      comment: "More lemon next time.",
    };
    const before = await rpc<CookingRecipe>(
      context,
      "cookingRecipe",
      identity,
      true,
    );
    expect(before).toMatchObject({
      recipe,
      servings: cart.servings,
      canReview: false,
      feedback: null,
    });
    expect((await raw(context, "saveRecipeFeedback", review)).status()).toBe(
      409,
    );
    for (const actor of [other, ops]) {
      expect((await raw(actor, "cookingRecipe", identity, true)).status()).toBe(
        404,
      );
      expect((await raw(actor, "saveRecipeFeedback", review)).status()).toBe(
        404,
      );
    }
    expect((await raw(guest, "cookingRecipe", identity, true)).status()).toBe(
      401,
    );
    expect(
      (
        await raw(
          context,
          "cookingRecipe",
          { ...identity, recipeId: "sesame-tofu" },
          true,
        )
      ).status(),
    ).toBe(404);
    for (const next of ["packing", "packed", "dispatched", "delivered"])
      await rpc(ops, "advance", { id: order.id, next });
    const saved = await rpc<RecipeFeedback>(
      context,
      "saveRecipeFeedback",
      review,
    );
    expect(await rpc(context, "saveRecipeFeedback", review)).toEqual(saved);
    expect(
      (
        await raw(context, "saveRecipeFeedback", { ...review, rating: 2 })
      ).status(),
    ).toBe(409);
    expect(
      (
        await raw(context, "saveRecipeFeedback", {
          ...review,
          requestKey: crypto.randomUUID(),
        })
      ).status(),
    ).toBe(409);
    expect(
      (
        await raw(context, "saveRecipeFeedback", {
          ...review,
          rating: 6,
          requestKey: crypto.randomUUID(),
        })
      ).ok(),
    ).toBe(false);
    const cleared = await rpc<RecipeFeedback>(context, "saveRecipeFeedback", {
      ...review,
      expectedVersion: saved.version,
      requestKey: crypto.randomUUID(),
      cookAgain: null,
    });
    expect(cleared.cookAgain).toBeNull();
    expect(
      (await rpc<CookingRecipe>(context, "cookingRecipe", identity, true))
        .feedback?.cookAgain,
    ).toBeNull();
    expect(
      (await raw(context, "recipeFeedback", { market: "us" }, true)).status(),
    ).toBe(404);

    await rpc(context, "preferences", { units: "us" });
    await visit(page, `/m/us/en/orders/${order.id}/cook/${recipe.id}`);
    await expect(
      page.getByRole("heading", { name: recipe.name, exact: true }),
    ).toBeVisible();
    await expect(page.getByRole("slider")).toHaveCount(0);
    await expect(
      page.getByRole("radio", { name: "US measures", exact: true }),
    ).toBeChecked();
    await expect(page.getByText("10.58 oz", { exact: true })).toBeVisible();
    await page.getByRole("radio", { name: "Metric", exact: true }).check();
    await expect(page.getByText("300 g", { exact: true })).toBeVisible();
    const ingredient = page.getByRole("checkbox", {
      name: `${recipe.ingredients[0]} 300 g`,
      exact: true,
    });
    await ingredient.check();
    await expect(ingredient).toBeChecked();
    await page
      .getByRole("button", { name: "Mark step 1 complete", exact: true })
      .click();
    await expect(
      page.getByRole("button", { name: "Mark step 1 incomplete", exact: true }),
    ).toHaveAttribute("aria-pressed", "true");
    await page.setViewportSize({ width: 390, height: 844 });
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    ).toBe(true);
    await page.emulateMedia({ media: "print" });
    await expect(page.getByText("300 g", { exact: true })).toBeVisible();
    await expect(ingredient).toBeHidden();
    await expect(page.locator("#step-1 p").first()).toHaveCSS(
      "text-decoration-line",
      "none",
    );
    await expect(
      page.getByRole("radio", { name: "Metric", exact: true }),
    ).toHaveCount(0);
    await page.emulateMedia({ media: "screen" });
    const comment = page.getByLabel("Anything you'd change? (optional)");
    await comment.fill("The vegetables were lovely.");
    await page.getByRole("radio", { name: "5 stars", exact: true }).check();
    const concurrent = await rpc<RecipeFeedback>(
      context,
      "saveRecipeFeedback",
      {
        ...review,
        expectedVersion: cleared.version,
        requestKey: crypto.randomUUID(),
        comment: "Saved on another device.",
      },
    );
    await page
      .getByRole("button", { name: "Update feedback", exact: true })
      .click();
    await expect(page.getByRole("alert")).toContainText(
      "Your feedback changed on another device.",
    );
    await expect(comment).toHaveValue("The vegetables were lovely.");
    await page
      .getByRole("button", { name: "Load saved feedback", exact: true })
      .click();
    await expect(comment).toHaveValue(concurrent.comment);
    await comment.fill("The vegetables were lovely.");
    await page.getByRole("radio", { name: "4 stars", exact: true }).focus();
    await page.keyboard.press("ArrowRight");
    await expect(
      page.getByRole("radio", { name: "5 stars", exact: true }),
    ).toBeChecked();
    await page
      .getByRole("button", { name: "Update feedback", exact: true })
      .click();
    await expect(page.getByRole("status")).toContainText(
      "Your feedback is saved.",
    );
    const after = await rpc<CookingRecipe>(
      context,
      "cookingRecipe",
      identity,
      true,
    );
    expect(after.feedback).toMatchObject({
      rating: 5,
      comment: "The vegetables were lovely.",
    });
    expect(after.recipe).toEqual(recipe);
    expect(after.servings).toBe(cart.servings);
    expect(
      (await rpc<Order>(context, "order", { id: order.id }, true)).snapshot,
    ).toEqual(order.snapshot);
    await page
      .getByRole("heading", { name: "How was this meal?", exact: true })
      .scrollIntoViewIfNeeded();
    await page.screenshot({
      path: test.info().outputPath("recipe-feedback-mobile.png"),
    });
    expect(
      (
        await new AxeBuilder({ page })
          .withTags(["wcag2a", "wcag2aa", "wcag21aa"])
          .analyze()
      ).violations,
    ).toEqual([]);
    await chooseOption(page.getByLabel("Language", { exact: true }), /中文/);
    await expect(
      page.getByRole("heading", { name: "这道菜怎么样？", exact: true }),
    ).toBeVisible();
    await expect(comment).toHaveCount(0);
    await expect(page.getByLabel("有什么改进建议？（选填）")).toHaveValue(
      "The vegetables were lovely.",
    );

    await visit(opsPage, "/m/us/en/operations");
    await opsPage
      .getByRole("tab", { name: "Recipe feedback", exact: true })
      .click();
    await expect(
      opsPage.getByText("The vegetables were lovely.", { exact: true }),
    ).toBeVisible();
    expect(await rpc(ops, "recipeFeedback", { market: "cn" }, true)).toEqual(
      [],
    );
    const privacy = await rpc<{ id: string }>(context, "requestPrivacy", {
      kind: "export",
      requestKey: crypto.randomUUID(),
    });
    const exported = await rpc<{ content: string }>(context, "privacyExport", {
      id: privacy.id,
    });
    expect(JSON.parse(exported.content).recipeFeedback).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          order_id: order.id,
          recipe_version_id: recipe.versionId,
          rating: 5,
        }),
      ]),
    );
  } finally {
    await ops.close();
    await other.close();
    await guest.close();
  }
});

test("step timers pause independently, survive navigation and return to their cooking step", async ({
  page,
}) => {
  await page.clock.install();
  await visit(page, "/m/us/en/recipes/lemon-chicken");
  await expect(
    page.getByRole("heading", { name: "Lemon & herb chicken", exact: true }),
  ).toBeVisible();
  const first = page.getByRole("group", {
    name: "Timer for step 1",
    exact: true,
  });
  const second = page.getByRole("group", {
    name: "Timer for step 2",
    exact: true,
  });
  await first.getByRole("button", { name: "Add timer" }).click();
  await first.getByLabel("Minutes", { exact: true }).fill("0");
  await first.getByLabel("Seconds", { exact: true }).fill("0");
  await expect(
    first.getByRole("button", { name: "Start timer" }),
  ).toBeDisabled();
  await first.getByLabel("Seconds", { exact: true }).fill("10");
  await first.getByRole("button", { name: "Start timer" }).click();
  await page.clock.fastForward(2000);
  await first.getByRole("button", { name: "Pause timer" }).click();
  const remaining = await first.getByRole("timer").innerText();
  await page.clock.fastForward(60_000);
  await expect(first.getByRole("timer")).toHaveText(remaining);
  await second.getByRole("button", { name: "Add timer" }).click();
  await second.getByLabel("Minutes", { exact: true }).fill("0");
  await second.getByLabel("Seconds", { exact: true }).fill("3");
  await second.getByRole("button", { name: "Start timer" }).click();
  await page.getByRole("link", { name: "Gather home", exact: true }).click();
  await page.clock.fastForward(4000);
  const kitchen = page.getByRole("region", {
    name: "Kitchen timers",
    exact: true,
  });
  await expect(kitchen.getByRole("status")).toHaveText("Timer finished");
  await kitchen
    .getByRole("link", { name: "Lemon & herb chicken · Step 2", exact: true })
    .click();
  await expect(page.locator("#step-2")).toBeFocused();
  await expect(page.locator("#step-2")).toBeInViewport();
  await expect(second).toContainText("Timer finished");
  await expect(first.getByRole("timer")).toHaveText(remaining);
  await first.getByRole("button", { name: "Resume timer" }).click();
  await page.clock.fastForward(10_000);
  await expect(first).toContainText("Timer finished");
  await first.getByRole("button", { name: "Reset timer" }).click();
  await expect(first.getByLabel("Minutes", { exact: true })).toHaveValue("0");
  await expect(first.getByLabel("Seconds", { exact: true })).toHaveValue("10");
  await first.getByRole("button", { name: "Start timer" }).click();
  await kitchen
    .getByRole("button", { name: "Dismiss timer for step 1" })
    .click();
  await kitchen
    .getByRole("button", { name: "Dismiss timer for step 2" })
    .click();
  await expect(kitchen).toHaveCount(0);
});

test("saved cooking units apply to recipes without reloading the app", async ({
  page,
  context,
}) => {
  await signIn(page);
  await rpc(context, "preferences", { units: "metric" });
  await visit(page, "/m/us/en/recipes/lemon-chicken");
  await expect(
    page.getByRole("radio", { name: "Metric", exact: true }),
  ).toBeChecked();
  await page.getByRole("button", { name: "My account", exact: true }).click();
  await page
    .getByRole("menuitem", { name: "Food preferences", exact: true })
    .click();
  await page.getByRole("radio", { name: "US measures", exact: true }).check();
  await page
    .getByRole("button", { name: "Save preferences", exact: true })
    .click();
  await expect(page.getByRole("status")).toContainText("Preferences saved.");
  await page
    .getByRole("navigation", { name: "Main navigation", exact: true })
    .getByRole("link", { name: "This week's menu", exact: true })
    .click();
  await page
    .getByRole("link", { name: "Lemon & herb chicken", exact: true })
    .first()
    .click();
  await expect(
    page.getByRole("radio", { name: "US measures", exact: true }),
  ).toBeChecked();
  await expect(page.getByText("10.58 oz", { exact: true })).toBeVisible();
});
