import {
  fail,
  cartSchema,
  addressSchema,
  type Cart,
  type Address,
} from "@gather/meal-kit/domain";

export function checkoutIdentity(cart: Cart, address: Address) {
  return JSON.stringify([cartSchema.parse(cart), addressSchema.parse(address)]);
}

export const checkoutHoldDurationMs = 15 * 60_000;
export function checkoutDeadline(cutoff: string, now = Date.now()) {
  const expires = Math.min(Date.parse(cutoff), now + checkoutHoldDurationMs);
  if (!Number.isFinite(expires) || expires <= now)
    fail(
      /* i18n */ "Ordering has closed for this delivery. Choose another date.",
      "CUTOFF",
      409,
    );
  return new Date(expires).toISOString();
}
export function reservationActive(
  attempt: { reservation: string; expires_at: string },
  now = Date.now(),
) {
  return (
    attempt.reservation === "active" && Date.parse(attempt.expires_at) > now
  );
}
