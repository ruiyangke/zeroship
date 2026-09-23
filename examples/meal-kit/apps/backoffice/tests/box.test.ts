import { describe, expect, it } from "vitest";
import { reviewBox } from "@gather/meal-kit/box-domain";
import { defaultCart } from "@gather/meal-kit/domain";
import { deliveryDates } from "@gather/meal-kit/catalog";
import { sampleMenu } from "./fixtures/catalog";

describe("customer box review", () => {
  const now = Date.parse("2026-09-11T12:00:00Z");
  const cart = {
    ...defaultCart("us"),
    deliveryDate: deliveryDates("us", now)[0],
    postal: "10001",
    recipeIds: ["lemon-chicken", "pesto-pasta", "miso-salmon"],
  };
  const menu = sampleMenu(cart, now);
  const catalog = {
    menu,
    availability: menu.recipes.map((recipe) => ({
      recipeId: recipe.id,
      available: 40,
      published: true,
    })),
  };

  it("requires available meals, matching box size and serviceable delivery", () => {
    expect(reviewBox(cart, catalog, now).ready).toBe(true);
    expect(
      reviewBox(
        { ...cart, recipeIds: cart.recipeIds.slice(0, 2) },
        catalog,
        now,
      ).ready,
    ).toBe(false);
    expect(reviewBox({ ...cart, mealCount: 2 }, catalog, now).ready).toBe(
      false,
    );
    expect(reviewBox({ ...cart, postal: "" }, catalog, now).ready).toBe(false);
    expect(
      reviewBox({ ...cart, deliveryDate: "2020-01-01" }, catalog, now)
        .dateAvailable,
    ).toBe(false);
    expect(reviewBox(cart, { ...catalog, menu: null }, now).ready).toBe(false);
    expect(
      reviewBox(cart, { ...catalog, menu: { ...menu, market: "cn" } }, now)
        .ready,
    ).toBe(false);
  });

  it("keeps invalid selections visible for repair instead of counting them as ready", () => {
    const removed = {
      ...catalog,
      menu: {
        ...menu,
        recipes: menu.recipes.filter((r) => r.id !== cart.recipeIds[0]),
      },
    };
    const unavailable = reviewBox(cart, removed, now);
    expect(unavailable.lines).toHaveLength(cart.recipeIds.length);
    expect(unavailable.lines[0]).toMatchObject({
      id: cart.recipeIds[0],
      problem: "No longer on this menu",
    });
    expect(unavailable.ready).toBe(false);
    expect(reviewBox(cart, { ...catalog, availability: [] }, now).ready).toBe(
      false,
    );
    expect(
      reviewBox(
        cart,
        {
          ...catalog,
          availability: catalog.availability.map((a) => ({
            ...a,
            available: 1,
          })),
        },
        now,
      ).ready,
    ).toBe(false);
    const excluded = reviewBox({ ...cart, exclude: ["fish"] }, catalog, now);
    expect(excluded.ready).toBe(false);
    expect(
      excluded.lines.find((line) => line.id === "miso-salmon")?.problem,
    ).toBe("Contains an ingredient you excluded");
    expect(reviewBox(cart, catalog, Date.parse(menu.closesAt)).ready).toBe(
      false,
    );
    expect(cart.recipeIds).toEqual([
      "lemon-chicken",
      "pesto-pasta",
      "miso-salmon",
    ]);
  });
});
