import { describe, expect, test } from "vitest";
import {
  ingredientAmount,
  feedbackContentSchema,
  purchasedRecipe,
} from "@gather/meal-kit/cooking-domain";
import {
  CookingTimers,
  pauseTimer,
  remainingTime,
  resumeTimer,
  startTimer,
} from "../src/cooking-timers";
import { defaultCart, type OrderSnapshot } from "@gather/meal-kit/domain";
import { sampleMenu } from "../../backoffice/tests/fixtures/catalog";

describe("cooking quantities and purchased content", () => {
  test("keeps mass and volume distinct and identifies US and UK fluid measures", () => {
    const mass = Object.freeze({ amount: 28.349523125, unit: "g" as const });
    expect(ingredientAmount(mass, 2, 2, "us")).toEqual({
      amount: 1,
      unit: "oz",
    });
    expect(ingredientAmount(mass, 4, 2, "imperial")).toEqual({
      amount: 2,
      unit: "oz",
    });
    expect(
      ingredientAmount({ amount: 29.5735295625, unit: "ml" }, 2, 2, "us"),
    ).toEqual({ amount: 1, unit: "us_fl_oz" });
    expect(
      ingredientAmount({ amount: 28.4130625, unit: "ml" }, 2, 2, "imperial"),
    ).toEqual({ amount: 1, unit: "uk_fl_oz" });
    expect(
      ingredientAmount({ amount: 100, unit: "ml" }, 2, 2, "us").amount,
    ).not.toBe(
      ingredientAmount({ amount: 100, unit: "ml" }, 2, 2, "imperial").amount,
    );
    for (const units of ["metric", "us", "imperial"] as const)
      expect(
        ingredientAmount({ amount: 1, unit: "piece" }, 4, 2, units),
      ).toEqual({ amount: 2, unit: "piece" });
    expect(ingredientAmount(mass, 2, 2, "metric")).toEqual(mass);
  });
  test("resolves only recipes recorded in the order and validates feedback", () => {
    const recipe = sampleMenu(defaultCart(), Date.now()).recipes[0];
    const order = { snapshot: { recipes: [recipe] } as OrderSnapshot };
    expect(purchasedRecipe(order, recipe.id)).toBe(recipe);
    expect(() => purchasedRecipe(order, "different-recipe")).toThrow(
      /Recipe not found/,
    );
    const valid = { rating: 5, cookAgain: null, comment: "  Great texture.  " };
    expect(feedbackContentSchema.parse(valid).comment).toBe("Great texture.");
    for (const rating of [0, 6, 2.5])
      expect(
        feedbackContentSchema.safeParse({ ...valid, rating }).success,
      ).toBe(false);
    expect(
      feedbackContentSchema.safeParse({ ...valid, comment: "x".repeat(2001) })
        .success,
    ).toBe(false);
  });
});

describe("kitchen timers", () => {
  test("uses deadlines after suspended ticks and preserves paused time", () => {
    const running = startTimer(10_000, 100_000);
    expect(remainingTime(running, 107_000)).toBe(3000);
    const paused = pauseTimer(running, 107_000);
    expect(remainingTime(paused, 900_000)).toBe(3000);
    const resumed = resumeTimer(paused, 900_000);
    expect(remainingTime(resumed, 902_000)).toBe(1000);
    expect(remainingTime(resumed, 950_000)).toBe(0);
    expect(pauseTimer(resumed, 950_000).status).toBe("finished");
    for (const duration of [0, -1000, NaN, Infinity, 1000.5, 181 * 60_000])
      expect(() => startTimer(duration, 0)).toThrow();
  });
  test("keeps independent timers across subscribers and clears them when identity changes", () => {
    let now = 0;
    const store = new CookingTimers(() => now);
    const context = {
      key: "first",
      step: 1,
      recipeId: "chicken",
      orderId: "owned-order",
      market: "us" as const,
      name: { en: "Chicken", zh: "Translated recipe" },
    };
    store.setOwner("customer");
    store.start(context, 10_000);
    store.start({ ...context, key: "second", step: 2 }, 3000);
    now = 2000;
    store.pause("first");
    now = 5000;
    store.tick();
    expect(
      store
        .getSnapshot()
        .map(({ status, remaining }) => ({ status, remaining })),
    ).toEqual([
      { status: "paused", remaining: 8000 },
      { status: "finished", remaining: 0 },
    ]);
    const snapshot = store.getSnapshot();
    const unsubscribe = store.subscribe(() => {});
    unsubscribe();
    expect(store.getSnapshot()).toBe(snapshot);
    store.setOwner("customer");
    expect(store.getSnapshot()).toBe(snapshot);
    store.resume("first");
    now = 20_000;
    store.tick();
    expect(
      store.getSnapshot().every((timer) => timer.status === "finished"),
    ).toBe(true);
    const finished = store.getSnapshot();
    store.tick();
    expect(store.getSnapshot()).toBe(finished);
    store.setOwner(null);
    expect(store.getSnapshot()).toEqual([]);
  });
});
