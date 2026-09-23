import { describe, expect, test } from "vitest";
import {
  checkoutIdentity,
  checkoutDeadline,
  checkoutHoldDurationMs,
  reservationActive,
} from "@gather/meal-kit/checkout-domain";
import { defaultCart } from "@gather/meal-kit/domain";

test("checkout identity survives object key reordering and still binds the recipient and cart", () => {
  const cart = defaultCart("cn");
  cart.area = { province: "shanghai", city: "shanghai", district: "pudong" };
  const address = {
    country: "CN" as const,
    province: "shanghai",
    city: "shanghai",
    district: "pudong",
    name: "Alex Morgan",
    email: "",
    line: "20 Garden Street",
    postal: "",
    phone: "13800138000",
    instructions: "",
  };
  const reordered = <T extends object>(value: T) =>
    Object.fromEntries(Object.entries(value).reverse()) as T;
  expect(
    checkoutIdentity(
      { ...reordered(cart), area: reordered(cart.area) },
      reordered(address),
    ),
  ).toBe(checkoutIdentity(cart, address));
  expect(
    checkoutIdentity(cart, { ...address, line: "88 Garden Street" }),
  ).not.toBe(checkoutIdentity(cart, address));
  expect(checkoutIdentity({ ...cart, servings: 4 }, address)).not.toBe(
    checkoutIdentity(cart, address),
  );
});

describe("checkout deadline", () => {
  const now = Date.parse("2026-09-11T12:00:00Z");
  test("ends at the reservation limit or the earlier menu cutoff", () => {
    expect(
      checkoutDeadline(
        new Date(now + checkoutHoldDurationMs * 2).toISOString(),
        now,
      ),
    ).toBe(new Date(now + checkoutHoldDurationMs).toISOString());
    expect(checkoutDeadline(new Date(now + 1000).toISOString(), now)).toBe(
      new Date(now + 1000).toISOString(),
    );
    expect(() => checkoutDeadline(new Date(now).toISOString(), now)).toThrow();
    expect(() => checkoutDeadline("invalid", now)).toThrow();
  });
  test("only an active unexpired reservation can confirm a box", () => {
    const attempt = {
      reservation: "active",
      expires_at: new Date(now + 1000).toISOString(),
    };
    expect(reservationActive(attempt, now)).toBe(true);
    expect(reservationActive(attempt, now + 1000)).toBe(false);
    for (const reservation of ["released", "expired", "consumed"])
      expect(reservationActive({ ...attempt, reservation }, now)).toBe(false);
    expect(reservationActive({ ...attempt, expires_at: "invalid" }, now)).toBe(
      false,
    );
  });
});
