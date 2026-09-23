import { z } from "zod";
import { menuIsOpen, type SaleMenu, type Recipe } from "@gather/meal-kit/catalog-domain";
import { markets, deliveryDates, type MarketId } from "@gather/meal-kit/catalog";

import {
  emptyArea,
  deliveryEligible,
  destinationKey,
  type Destination,
} from "@gather/meal-kit/countries";

export const areaSchema = z.object({
  province: z.string().max(80),
  city: z.string().max(80),
  district: z.string().max(80),
});
export const marketSchema = z.enum(["us", "uk", "cn"]);
export const servingRange = { min: 1, max: 10 } as const;
export const cartSchema = z.object({
  market: marketSchema,
  servings: z.number().int().min(servingRange.min).max(servingRange.max),
  mealCount: z.union([z.literal(2), z.literal(3), z.literal(4)]),
  recipeIds: z.array(z.string()).max(4),
  deliveryDate: z.string().regex(/^\d{4}-\d{2}-\d{2}$/),
  postal: z.string().max(20),
  area: areaSchema,
  recurring: z.boolean(),
  exclude: z
    .array(z.enum(["milk", "wheat", "nuts", "fish", "soy", "sesame"]))
    .max(6)
    .default([]),
});
export type Cart = z.infer<typeof cartSchema>;
export const addressSchema = z
  .object({
    country: z.enum(["US", "GB", "CN"]),
    province: z.string().trim().max(80),
    district: z.string().trim().max(80),
    name: z.string().trim().min(2).max(100),
    email: z
      .string()
      .max(200)
      .refine(
        (value) => value === "" || z.string().email().safeParse(value).success,
      ),
    line: z.string().trim().min(5).max(200),
    city: z.string().trim().min(2).max(100),
    postal: z.string().trim().max(20),
    phone: z.string().trim().min(5).max(30),
    instructions: z.string().max(500).default(""),
  })
  .superRefine((address, ctx) => {
    if (address.country === "CN") {
      if (!address.province || !address.district)
        ctx.addIssue({
          code: "custom",
          path: ["district"],
          message: "Choose the province, city and district.",
        });
      if (
        !/^(?:\+?86[ -]?)?1[3-9]\d{9}$/.test(address.phone.replace(/\s/g, ""))
      )
        ctx.addIssue({
          code: "custom",
          path: ["phone"],
          message: "Enter a mobile number.",
        });
    } else if (!address.postal)
      ctx.addIssue({
        code: "custom",
        path: ["postal"],
        message: "Enter a postal code.",
      });
    if (address.country !== "CN" && !address.email)
      ctx.addIssue({
        code: "custom",
        path: ["email"],
        message: "Enter an email address.",
      });
    if (address.country === "US" && !address.province)
      ctx.addIssue({
        code: "custom",
        path: ["province"],
        message: "Enter a state.",
      });
  });
export type Address = z.infer<typeof addressSchema>;
export function addressValidationMessage(error: z.ZodError) {
  switch (error.issues[0]?.path[0]) {
    case "name":
      return /* i18n */ "Enter the recipient's full name.";
    case "email":
      return /* i18n */ "Enter a valid email address.";
    case "line":
      return /* i18n */ "Enter the street address, including the building or apartment.";
    case "city":
      return /* i18n */ "Choose or enter your city.";
    case "province":
      return /* i18n */ "Choose or enter your province or state.";
    case "district":
      return /* i18n */ "Choose your district.";
    case "postal":
      return /* i18n */ "Enter a valid postal code.";
    case "phone":
      return /* i18n */ "Enter a valid phone number.";
    default:
      return /* i18n */ "Check your address and contact details, then try again.";
  }
}
export type Quote = {
  token?: string;
  menuVersionId: string;
  subtotal: number;
  premium: number;
  shipping: number;
  total: number;
  currency: string;
  fingerprint: string;
  expiresAt: string;
};
export type OrderSnapshot = {
  cart: Cart;
  address: Address;
  quote: Quote;
  recipes: Recipe[];
  cutoff: string;
  policyVersion: string;
  consentAt: string;
  paymentAttemptId?: string;
  paymentDeadline?: string;
};
export type TimelineEvent = {
  at: string;
  key: string;
  detail: string;
  values?: Record<string, string | number>;
};
export type Order = {
  id: string;
  version: number;
  market: MarketId;
  status: string;
  payment: string;
  fulfillment: string;
  total: number;
  refunded: number;
  snapshot: OrderSnapshot;
  timeline: TimelineEvent[];
};
export function fail(
  message: string,
  code = "INVALID_REQUEST",
  status = 400,
): never {
  throw Object.assign(new Error(message), { code, status });
}
export function addressDestination(address: Address): Destination {
  return {
    postal: address.postal,
    area: {
      province: address.province,
      city: address.city,
      district: address.district,
    },
  };
}
export function validateAddress(address: Address, market: MarketId) {
  const parsed = addressSchema.parse(address);
  if (parsed.country !== markets[market].country)
    fail(
      /* i18n */ "Choose an address in the selected delivery country.",
      "ADDRESS_COUNTRY_MISMATCH",
    );
  if (!deliveryEligible(market, addressDestination(parsed)))
    fail(/* i18n */ "We don't deliver to this address yet.", "OUTSIDE_AREA");
  return parsed;
}
export function validateCart(
  input: Cart,
  menu: SaleMenu,
  now = Date.now(),
  complete = true,
) {
  const cart = cartSchema.parse(input);
  if (complete && cart.recipeIds.length !== cart.mealCount)
    fail(
      /* i18n */ "Finish choosing your meals before continuing.",
      "INCOMPLETE_BOX",
    );
  if (new Set(cart.recipeIds).size !== cart.recipeIds.length)
    fail(/* i18n */ "Choose each recipe only once.");
  if (!deliveryEligible(cart.market, cart))
    fail(/* i18n */ "We don't deliver to this address yet.", "OUTSIDE_AREA");
  if (!deliveryDates(cart.market, now).includes(cart.deliveryDate))
    fail(
      /* i18n */ "Choose an available delivery date.",
      "DELIVERY_UNAVAILABLE",
    );
  if (
    menu.market !== cart.market ||
    menu.date !== cart.deliveryDate ||
    !menuIsOpen(menu, now)
  )
    fail(
      /* i18n */ "This menu is no longer available. Choose another delivery date.",
      "MENU_UNAVAILABLE",
      409,
    );
  for (const id of cart.recipeIds) {
    const recipe = menu.recipes.find((r) => r.id === id);
    if (!recipe)
      fail(
        /* i18n */ "This meal is unavailable in your delivery area.",
        "WRONG_MARKET_OFFERING",
      );
    if (
      recipe.allergens.some((a) =>
        cart.exclude.includes(a as Cart["exclude"][number]),
      )
    )
      fail(
        /* i18n */ "One of your meals contains an ingredient you chose to avoid.",
        "ALLERGEN_CONFLICT",
      );
  }
  return cart;
}
export function quoteCart(cart: Cart, menu: SaleMenu, now = Date.now()): Quote {
  validateCart(cart, menu, now);
  const market = menu;
  const subtotal = market.price * cart.servings * cart.recipeIds.length;
  const premium =
    cart.recipeIds.reduce(
      (n, id) => n + menu.recipes.find((recipe) => recipe.id === id)!.premium,
      0,
    ) * cart.servings;
  const total = subtotal + premium + market.shipping;
  const fingerprint = JSON.stringify([
    cart.market,
    menu.id,
    cart.recipeIds
      .map((id) => menu.recipes.find((recipe) => recipe.id === id)!.versionId)
      .sort(),
    cart.servings,
    [...cart.recipeIds].sort(),
    cart.deliveryDate,
    destinationKey(cart.market, cart),
    cart.recurring,
    total,
    [...cart.exclude].sort(),
  ]);
  return {
    menuVersionId: menu.id,
    subtotal,
    premium,
    shipping: market.shipping,
    total,
    currency: market.currency,
    fingerprint,
    expiresAt: new Date(now + 15 * 60_000).toISOString(),
  };
}
export function assertQuote(
  cart: Cart,
  quote: Quote,
  menu: SaleMenu,
  now = Date.now(),
) {
  const actual = quoteCart(cart, menu, now);
  if (
    actual.menuVersionId !== quote.menuVersionId ||
    actual.fingerprint !== quote.fingerprint ||
    actual.total !== quote.total ||
    actual.subtotal !== quote.subtotal ||
    actual.premium !== quote.premium ||
    actual.shipping !== quote.shipping ||
    actual.currency !== quote.currency ||
    !Number.isFinite(Date.parse(quote.expiresAt)) ||
    Date.parse(quote.expiresAt) <= now
  )
    fail(
      /* i18n */ "Your box has changed. Review the updated total.",
      "QUOTE_CHANGED",
      409,
    );
  return quote;
}
export const transitions: Record<string, string[]> = {
  unallocated: ["packing"],
  packing: ["packed"],
  packed: ["dispatched"],
  dispatched: ["delivered", "exception"],
  exception: ["dispatched", "delivered"],
  delivered: [],
};
export function checkTransition(order: Order, next: string) {
  if (
    order.payment !== "succeeded" ||
    !["confirmed", "completed"].includes(order.status)
  )
    fail(
      /* i18n */ "Payment must be confirmed before fulfillment.",
      "PAYMENT_REQUIRED",
      409,
    );
  if (!transitions[order.fulfillment]?.includes(next))
    fail(
      /* i18n */ "That fulfillment transition is not available.",
      "INVALID_TRANSITION",
      409,
    );
}
export function checkEditable(order: Order, now = Date.now()) {
  if (
    order.status === "canceled" ||
    order.fulfillment !== "unallocated" ||
    now >= Date.parse(order.snapshot.cutoff)
  )
    fail(
      /* i18n */ "This box is locked for preparation. Please contact support.",
      "BOX_LOCKED",
      409,
    );
}
export function defaultCart(market: MarketId = "us"): Cart {
  return {
    market,
    servings: 2,
    mealCount: 3,
    recipeIds: [],
    deliveryDate: deliveryDates(market)[0],
    postal: "",
    area: emptyArea(),
    recurring: true,
    exclude: [],
  };
}
