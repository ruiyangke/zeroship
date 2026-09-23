import { sampleMenu } from "./fixtures/catalog";
import { recipesForMarket } from "../src/seed-catalog";
import { expect, test } from "vitest";
import { cutoffForDate, deliveryDates, markets } from "@gather/meal-kit/catalog";
import {
  addressSchema,
  defaultCart,
  quoteCart,
  validateAddress,
  validateCart,
} from "@gather/meal-kit/domain";
import { deliveryEligible, emptyArea } from "@gather/meal-kit/countries";

const now = Date.parse("2026-09-11T10:00:00Z");
const area = { province: "shanghai", city: "shanghai", district: "pudong" };
const address = {
  country: "CN" as const,
  province: "shanghai",
  city: "shanghai",
  district: "pudong",
  postal: "",
  name: "Alex Morgan",
  email: "alex@example.com",
  line: "88 Garden Road, Building A, Apartment 501",
  phone: "13800138000",
  instructions: "",
};

test("China requires the administrative delivery area and mobile number, not a postcode", () => {
  expect(deliveryEligible("cn", { postal: "", area })).toBe(true);
  expect(validateAddress(address, "cn")).toEqual(address);
  expect(deliveryEligible("cn", { postal: "200000", area: emptyArea() })).toBe(
    false,
  );
  expect(
    deliveryEligible("cn", {
      postal: "200000",
      area: { ...area, district: "other" },
    }),
  ).toBe(false);
  expect(() =>
    validateAddress({ ...address, district: "other" }, "cn"),
  ).toThrow(/deliver/);
  expect(() => validateAddress(address, "us")).toThrow(/country/);
  expect(addressSchema.safeParse({ ...address, phone: "12345" }).success).toBe(
    false,
  );
  expect(addressSchema.safeParse({ ...address, district: "" }).success).toBe(
    false,
  );
  const us = {
    ...address,
    country: "US" as const,
    province: "NY",
    city: "New York",
    district: "",
    postal: "10001",
    phone: "+12125550123",
  };
  expect(validateAddress(us, "us")).toEqual(us);
  expect(addressSchema.safeParse({ ...us, postal: "" }).success).toBe(false);
  expect(addressSchema.safeParse({ ...us, province: "" }).success).toBe(false);
});

test("the country selects its offerings and price book and a changed area invalidates the quote", () => {
  const cart = {
    ...defaultCart("cn"),
    area,
    deliveryDate: deliveryDates("cn", now)[0],
    recipeIds: recipesForMarket("cn")
      .slice(0, 3)
      .map((recipe) => recipe.id),
  };
  expect(validateCart(cart, sampleMenu(cart, now), now)).toEqual(cart);
  const quote = quoteCart(cart, sampleMenu(cart, now), now);
  expect(quote.currency).toBe("CNY");
  expect(quote.premium).toBe(2000);
  expect(quote.total).toBe(
    markets.cn.price * cart.servings * cart.mealCount +
      markets.cn.shipping +
      2000,
  );
  expect(
    quoteCart(
      { ...cart, area: { ...area, district: "xuhui" } },
      sampleMenu({ ...cart, area: { ...area, district: "xuhui" } }, now),
      now,
    ).fingerprint,
  ).not.toBe(quote.fingerprint);
  expect(
    quoteCart(
      { ...cart, postal: "200000" },
      sampleMenu({ ...cart, postal: "200000" }, now),
      now,
    ).fingerprint,
  ).toBe(quote.fingerprint);
  expect(() =>
    validateCart(
      { ...cart, recipeIds: ["lemon-chicken", "pesto-pasta", "miso-salmon"] },
      sampleMenu(
        { ...cart, recipeIds: ["lemon-chicken", "pesto-pasta", "miso-salmon"] },
        now,
      ),
      now,
    ),
  ).toThrow(/unavailable/);
  expect(
    recipesForMarket("us").some((recipe) => recipe.id === "pesto-pasta"),
  ).toBe(true);
  expect(
    recipesForMarket("uk").some((recipe) => recipe.id === "sesame-tofu"),
  ).toBe(false);
});

test("delivery calendars and cutoff instants follow each market and daylight-saving changes", () => {
  expect(deliveryDates("cn", now)).toContain("2026-09-17");
  expect(deliveryDates("us", now)).not.toContain("2026-09-17");
  expect(deliveryDates("uk", now)).toContain("2026-09-16");
  expect(cutoffForDate("cn", "2026-09-15")).toBe("2026-09-13T10:00:00.000Z");
  expect(cutoffForDate("us", "2026-03-06")).toBe("2026-03-04T23:00:00.000Z");
  expect(cutoffForDate("us", "2026-03-10")).toBe("2026-03-08T22:00:00.000Z");
  expect(cutoffForDate("uk", "2026-03-28")).toBe("2026-03-26T18:00:00.000Z");
  expect(cutoffForDate("uk", "2026-04-01")).toBe("2026-03-30T17:00:00.000Z");
  const boundary = Date.parse(cutoffForDate("cn", "2026-09-15"));
  expect(deliveryDates("cn", boundary - 1)).toContain("2026-09-15");
  expect(deliveryDates("cn", boundary)).not.toContain("2026-09-15");
});
