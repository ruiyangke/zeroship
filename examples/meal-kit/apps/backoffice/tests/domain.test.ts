import { sampleMenu } from "./fixtures/catalog";
import { recipes, recipesForMarket } from "../src/seed-catalog";
import { deliveryEligible, emptyArea } from "@gather/meal-kit/countries";
import { describe, expect, test } from "vitest";
import {
  deliveryDates,
  cutoffForDate,
  markets,
  type MarketId,
} from "@gather/meal-kit/catalog";
import {
  assertQuote,
  checkEditable,
  checkTransition,
  defaultCart,
  cartSchema,
  servingRange,
  quoteCart,
  validateCart,
  type Cart,
  type Order,
} from "@gather/meal-kit/domain";

const now = Date.parse("2026-09-11T10:00:00Z");
const cart = (market: MarketId = "us"): Cart => ({
  ...defaultCart(market),
  postal: markets[market].postal,
  deliveryDate: deliveryDates(market, now)[0],
  area:
    market === "cn"
      ? { province: "shanghai", city: "shanghai", district: "pudong" }
      : emptyArea(),
  recipeIds: recipesForMarket(market)
    .slice(0, 3)
    .map((r) => r.id),
});
describe("box validation and prices", () => {
  test("quotes every supported portion count and rejects invalid quantities", () => {
    const input = cart();
    const menu = sampleMenu(input, now);
    const one = quoteCart({ ...input, servings: 1 }, menu, now);
    for (
      let servings = servingRange.min;
      servings <= servingRange.max;
      servings++
    ) {
      expect(cartSchema.parse({ ...input, servings }).servings).toBe(servings);
      const quote = quoteCart({ ...input, servings }, menu, now);
      expect(quote.total).toBe(
        (one.total - one.shipping) * servings + one.shipping,
      );
    }
    for (const servings of [servingRange.min - 1, servingRange.max + 1, 1.5])
      expect(() => quoteCart({ ...input, servings }, menu, now)).toThrow();
  });
  test.each(["us", "uk", "cn"] as const)(
    "quotes local prices and rejects cross-market quotes for %s",
    (market) => {
      const input = cart(market);
      const quote = quoteCart(input, sampleMenu(input, now), now);
      expect(quote.total).toBe(
        markets[market].price * 6 +
          markets[market].shipping +
          (market === "cn" ? 2000 : market === "uk" ? 600 : 500),
      );
      expect(quote.currency).toBe(markets[market].currency);
      expect(
        assertQuote(input, quote, sampleMenu(input, now), now),
      ).toMatchObject({
        total: quote.total,
      });
      expect(() =>
        assertQuote(
          input,
          { ...quote, currency: "INVALID" },
          sampleMenu(input, now),
          now,
        ),
      ).toThrow(/updated total/);
      expect(() =>
        assertQuote(input, { ...quote, total: 1 }, sampleMenu(input, now), now),
      ).toThrow(/updated total/);
      expect(() =>
        assertQuote(
          input,
          { ...quote, expiresAt: "not-a-date" },
          sampleMenu(input, now),
          now,
        ),
      ).toThrow(/updated total/);
      expect(() =>
        assertQuote(
          input,
          quote,
          sampleMenu(input, now),
          Date.parse(quote.expiresAt),
        ),
      ).toThrow(/updated total/);
    },
  );
  test("rejects incomplete, duplicate, unavailable and allergen-conflicting selections", () => {
    const input = cart();
    expect(validateCart(input, sampleMenu(input, now), now)).toEqual(input);
    for (const next of [
      { ...input, recipeIds: [] },
      {
        ...input,
        recipeIds: ["lemon-chicken", "lemon-chicken", "miso-salmon"],
      },
      { ...input, recipeIds: ["unknown", "miso-salmon", "pesto-pasta"] },
      { ...input, exclude: ["fish"] as Cart["exclude"] },
      { ...input, postal: "94103" },
      { ...input, deliveryDate: "2020-01-01" },
    ])
      expect(() => validateCart(next, sampleMenu(next, now), now)).toThrow();
  });
  test("keeps an offered date available across midnight until its cutoff", () => {
    const before = Date.parse("2026-09-11T23:59:00Z");
    const after = Date.parse("2026-09-12T00:01:00Z");
    const dates = deliveryDates("us", before);
    expect(dates.length).toBeGreaterThan(0);
    for (const date of dates)
      expect(deliveryDates("us", after)).toContain(date);
    const delivery = dates[0];
    const cutoff = Date.parse(cutoffForDate("us", delivery));
    expect(deliveryDates("us", cutoff - 1)).toContain(delivery);
    expect(deliveryDates("us", cutoff)).not.toContain(delivery);
  });
  test("normalizes service-area input and rejects unsupported postal zones", () => {
    expect(
      deliveryEligible("uk", { postal: "sw1a 1aa", area: emptyArea() }),
    ).toBe(true);
    expect(
      deliveryEligible("cn", {
        postal: "",
        area: { province: "shanghai", city: "shanghai", district: "pudong" },
      }),
    ).toBe(true);
    expect(
      deliveryEligible("cn", { postal: "100000", area: emptyArea() }),
    ).toBe(false);
    expect(
      deliveryEligible("us", { postal: "SW1A 1AA", area: emptyArea() }),
    ).toBe(false);
    expect(
      deliveryEligible("us", { postal: "10001anything", area: emptyArea() }),
    ).toBe(false);
  });
});
describe("order controls", () => {
  const input = cart();
  const order: Order = {
    id: "test",
    version: 1,
    market: "us",
    status: "confirmed",
    payment: "succeeded",
    fulfillment: "unallocated",
    total: 100,
    refunded: 0,
    snapshot: {
      cart: input,
      address: {
        country: "US",
        province: "NY",
        district: "",
        name: "Alex",
        email: "alex@example.com",
        line: "Garden Street",
        city: "New York",
        postal: "10001",
        phone: "1234567",
        instructions: "",
      },
      quote: quoteCart(input, sampleMenu(input, now), now),
      recipes: sampleMenu(input, now).recipes.slice(0, 3),
      policyVersion: "us-demo-v1",
      cutoff: new Date(now + 1000).toISOString(),
      consentAt: new Date(now).toISOString(),
    },
    timeline: [],
  };
  test("allows preparation only for paid boxes and ordered transitions", () => {
    expect(() => checkTransition(order, "packing")).not.toThrow();
    expect(() => checkTransition(order, "delivered")).toThrow();
    expect(() =>
      checkTransition({ ...order, payment: "failed" }, "packing"),
    ).toThrow();
    expect(() =>
      checkTransition({ ...order, status: "canceled" }, "packing"),
    ).toThrow();
    expect(() =>
      checkTransition({ ...order, status: "payment_recovery" }, "packing"),
    ).toThrow();
  });
  test("locks changes at cutoff and after fulfillment starts", () => {
    expect(() => checkEditable(order, now)).not.toThrow();
    expect(() => checkEditable(order, now + 1000)).toThrow();
    expect(() =>
      checkEditable({ ...order, fulfillment: "packing" }, now),
    ).toThrow();
    expect(() =>
      checkEditable({ ...order, status: "canceled" }, now),
    ).toThrow();
  });
});
