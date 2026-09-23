"use server";

// The storefront's server surface: everything a customer does.
//
// Every one of these reads and writes `env.db`, which the workspace config
// binds to `databases.main` - the same database the back office binds. A box
// placed here is on the operations board the moment the transaction commits;
// there is no call between the apps.

import { env } from "zeroship";
import { auth } from "@zeroship/auth";
import type { Id } from "@zeroship/db";
import { kv } from "@zeroship/kv";
import { bucket } from "@zeroship/storage";
import {
  Workflow,
  type Step,
  type WorkflowTrigger,
  type WorkflowRun,
} from "@zeroship/workflows";
import { mutation, query } from "@zeroship/rpc/server";
import { z } from "zod";
import {
  markets,
  deliveryDates,
  type MarketId,
} from "@gather/meal-kit/catalog";
import {
  cartSchema,
  addressSchema,
  marketSchema,
  quoteCart,
  assertQuote,
  validateAddress,
  addressDestination,
  areaSchema,
  fail,
  checkEditable,
  type Cart,
  type Order,
  type OrderSnapshot,
  type TimelineEvent,
  type Quote,
} from "@gather/meal-kit/domain";
import {
  must,
  user,
  transact,
  changed,
  demo,
  type Tx,
} from "@gather/meal-kit/server/core";
import { staffAccess } from "@gather/meal-kit/server/staff-access";
import { planChange } from "@gather/meal-kit/account-domain";
import { checkoutIdentity } from "@gather/meal-kit/checkout-domain";
import { destinationKey, countryPolicies } from "@gather/meal-kit/countries";
import { publishedMenu, sellableMenu } from "@gather/meal-kit/server/catalog-store";
import {
  expireReservations,
  startAttempt,
  settleAttempt,
  releasePendingAttempt,
  requirePaymentReady,
} from "@gather/meal-kit/server/checkout-store";
import { event, wire, owned, adjustStock } from "@gather/meal-kit/server/orders";
import { menuIsOpen, type SaleMenu } from "@gather/meal-kit/catalog-domain";
import { consumeDraft } from "./lib/draft-store";
import { draftFetch } from "./lib/draft-http";
import { draftSessionFetch } from "./lib/draft-session";

const quoteInputSchema = z.object({
  token: z.string().uuid(),
  menuVersionId: z.string(),
  subtotal: z.number().int(),
  premium: z.number().int(),
  shipping: z.number().int(),
  total: z.number().int(),
  currency: z.string(),
  fingerprint: z.string(),
  expiresAt: z.string(),
});

async function trustedQuote(
  tx: Tx,
  owner: string,
  cart: Cart,
  quote: Quote,
  menu: SaleMenu,
) {
  const saved = await tx.meal_events.get({ event_key: "quote:" + quote.token });
  const payload = saved?.payload as { owner: string; quote: Quote } | undefined;
  if (
    !payload ||
    payload.owner !== owner ||
    JSON.stringify(quoteInputSchema.parse(payload.quote)) !==
      JSON.stringify(quoteInputSchema.parse(quote))
  )
    fail(
      /* i18n */ "Refresh your total before placing this order.",
      "QUOTE_CHANGED",
      409,
    );
  return assertQuote(cart, payload.quote, menu);
}
type ReceiptInput = { key: string; document: string };
type ReceiptOutput = { key: string };
function receiptWorkflow() {
  return (
    env.workflows as {
      OrderRecord: {
        start(options: {
          input: ReceiptInput;
          key: string;
          onConflict: "join";
        }): Promise<WorkflowRun<ReceiptOutput>>;
      };
    }
  ).OrderRecord;
}

export class OrderRecord extends Workflow<ReceiptInput, ReceiptOutput> {
  async run(trigger: WorkflowTrigger<ReceiptInput>, step: Step) {
    const receipt = trigger.input;
    await step.run("store-receipt", async () => {
      must(
        await bucket("gather-receipts").put(receipt.key, receipt.document, {
          contentType: "application/json",
        }),
      );
    });
    return { key: receipt.key };
  }
}

// The object the workflow writes IS the completion record, and it is what
// this reads. A workflow key only deduplicates starts while its run is live -
// `finish_run` clears the key when the run goes terminal, so a poll that
// arrives after the work is done joins nothing and would start the job again.
// Asking storage first makes the poll idempotent and leaves the key doing the
// one job it can do: collapsing the starts that race inside a single run.
export const downloadReceipt = mutation(
  async ({ id }: { id: string }) => {
    const row = await owned(id);
    const key = `${row.owner_id}/${row.id}/${row.version}.json`;
    const stored = must(await bucket("gather-receipts").get(key));
    if (stored)
      return {
        ready: true as const,
        content: new TextDecoder().decode(stored.bytes),
      };
    const run = await receiptWorkflow().start({
      input: {
        key,
        document: JSON.stringify({ kind: "demo_receipt", ...wire(row) }, null, 2),
      },
      key: `${row.id}:${row.version}`,
      onConflict: "join",
    });
    if ((await run.status()).state === "failed")
      fail(
        /* i18n */ "We couldn't prepare your download. Please contact us.",
        "RECEIPT_FAILED",
        503,
      );
    return { ready: false as const, content: "" };
  },
  { id: "gather.receipt", input: z.object({ id: z.string() }) },
);

// `staff` comes out of the SAME `meal_staff_members` table the back office
// writes: a staff member signing in here is offered the operations link
// because both apps read one database.
export const getSession = query(
  async () => {
    const u = auth.getUser();
    return {
      user: u
        ? { id: u.id, name: u.name ?? "Guest", email: u.email ?? "" }
        : null,
      staff: u ? await staffAccess(u.id) : null,
      mode: "demo" as const,
    };
  },
  { id: "gather.session" },
);
export const getCatalog = query(
  async ({ market, date }: { market: MarketId; date: string }) =>
    transact(async (tx) => {
      await expireReservations(tx, market);
      const published = await publishedMenu(tx, market, date);
      const menu = published && menuIsOpen(published) ? published : null;
      const rows = await tx.meal_inventory.find({ market });
      const dates: string[] = [];
      for (const candidate of deliveryDates(market)) {
        const capacity = rows.find(
          (row) => row.stock_key === `${market}:${candidate}:delivery`,
        );
        if (!capacity?.published || capacity.available <= 0) continue;
        const offering =
          candidate === date
            ? menu
            : await publishedMenu(tx, market, candidate);
        if (offering && menuIsOpen(offering)) dates.push(candidate);
      }
      return {
        menu,
        recipes: menu?.recipes ?? [],
        markets,
        dates,
        availability: (menu?.recipes ?? []).map((recipe) => {
          const row = rows.find(
            (stock) =>
              stock.stock_key === market + ":" + date + ":" + recipe.id,
          );
          const delivery = rows.find(
            (stock) => stock.stock_key === market + ":" + date + ":delivery",
          );
          return {
            recipeId: recipe.id,
            available:
              delivery?.published && delivery.available > 0
                ? (row?.available ?? 0)
                : 0,
            published: Boolean(row?.published && delivery?.published),
          };
        }),
      };
    }),
  {
    id: "gather.catalog",
    input: z.object({ market: marketSchema, date: z.string() }),
  },
);
export const getQuote = mutation(
  async (cart: Cart) => {
    const u = user();
    const count = must(
      await kv.incr(`gather:quotes:${u.id}`, { ttlMs: 60_000 }),
    );
    if (count > 30)
      fail(
        /* i18n */ "Please wait a moment, then try refreshing your total.",
        "RATE_LIMITED",
        429,
      );
    return transact(async (tx) => {
      const menu = await sellableMenu(tx, cart.market, cart.deliveryDate);
      const quote = { ...quoteCart(cart, menu), token: crypto.randomUUID() };
      await tx.meal_events.insert({
        event_key: "quote:" + quote.token,
        kind: "quote",
        payload: { owner: u.id, quote },
      });
      return quote;
    });
  },
  {
    id: "gather.quote",
    input: cartSchema,
  },
);
export const joinWaitlist = mutation(
  async ({
    email,
    market,
    postal,
    area,
  }: {
    email: string;
    market: MarketId;
    postal: string;
    area: z.infer<typeof areaSchema>;
    consent: true;
  }) => {
    const contact_key = `${market}:${email.toLowerCase()}`;
    const existing = must(await env.db.meal_waitlist.get({ contact_key }));
    if (!existing)
      must(
        await env.db.meal_waitlist.insert({
          contact_key,
          email,
          market,
          postal,
          area,
        }),
      );
    return { saved: true };
  },
  {
    id: "gather.waitlist",
    input: z.object({
      email: z.string().email().max(200),
      market: marketSchema,
      postal: z.string().max(20),
      area: areaSchema,
      consent: z.literal(true),
    }),
  },
);

export const checkout = mutation(
  async ({
    cart,
    address,
    quote,
    requestKey,
    consent,
  }: {
    cart: Cart;
    address: z.infer<typeof addressSchema>;
    quote: Quote;
    requestKey: string;
    consent: true;
  }) => {
    demo();
    const u = user();
    if (!consent) fail(/* i18n */ "Please review and accept the order terms.");
    validateAddress(address, cart.market);
    if (
      destinationKey(cart.market, addressDestination(address)) !==
      destinationKey(cart.market, cart)
    )
      fail(
        /* i18n */ "Your delivery area changed. Review your box and total again.",
        "ADDRESS_MISMATCH",
      );
    const request_key = `${u.id}:${requestKey}`;
    return transact(async (tx) => {
      await expireReservations(tx, cart.market);
      const previous = await tx.meal_orders.get({ request_key });
      if (previous) {
        const snapshot = previous.snapshot as OrderSnapshot;
        if (
          checkoutIdentity(snapshot.cart, snapshot.address) !==
            checkoutIdentity(cart, address) ||
          snapshot.quote.fingerprint !== quote.fingerprint ||
          snapshot.quote.currency !== quote.currency ||
          snapshot.quote.total !== quote.total
        )
          fail(
            /* i18n */ "This checkout has already been used. Please start again from your box.",
            "KEY_REUSED",
            409,
          );
        return wire(previous);
      }
      await requirePaymentReady(tx, u.id, cart.market);
      const menu = await sellableMenu(tx, cart.market, cart.deliveryDate);
      const authoritative = await trustedQuote(tx, u.id, cart, quote, menu);
      const scenario = await tx.meal_events.get({
        event_key: `scenario:${u.id}:${cart.market}`,
      });
      const selected = scenario?.payload as
        | {
            outcome: "succeeded" | "failed" | "requires_action" | "processing";
            expiresAt: number;
          }
        | undefined;
      const outcome =
        selected && selected.expiresAt > Date.now()
          ? selected.outcome
          : "succeeded";
      if (scenario) await tx.meal_events.delete(scenario.id);
      const snapshot: OrderSnapshot = {
        cart,
        address,
        quote: authoritative,
        recipes: menu.recipes.filter((r) => cart.recipeIds.includes(r.id)),
        cutoff: menu.closesAt,
        policyVersion: countryPolicies[cart.market].version,
        consentAt: new Date().toISOString(),
      };
      let order = await tx.meal_orders.insert({
        owner_id: u.id,
        market: cart.market,
        request_key,
        status: "pending_payment",
        payment: "processing",
        fulfillment: "unallocated",
        total: authoritative.total,
        refunded: 0,
        snapshot,
        timeline: [
          event("created", /* i18n */ "Box created"),
          event(
            "payment_started",
            /* i18n */ "Waiting for payment confirmation",
          ),
        ],
      });
      order = await startAttempt(
        tx,
        order,
        outcome === "requires_action" ? "requires_action" : "processing",
      );
      if (outcome === "succeeded" || outcome === "failed")
        order = await settleAttempt(
          tx,
          (order.snapshot as OrderSnapshot).paymentAttemptId!,
          outcome,
        );
      await consumeDraft(tx, u.id, cart);
      return wire(order);
    });
  },
  {
    id: "gather.checkout",
    input: z.object({
      cart: cartSchema,
      address: addressSchema,
      quote: quoteInputSchema,
      requestKey: z.string().uuid(),
      consent: z.literal(true),
    }),
  },
);

export const getAccount = query(
  async ({ market }: { market: MarketId }) => {
    const owner_id = user().id;
    await transact((tx) => expireReservations(tx, market));
    const orders = must(
      await env.db.meal_orders
        .find({ owner_id, market })
        .sort({ created_at: -1 }),
    );
    const plan = must(await env.db.meal_plans.get({ owner_id, market }));
    const orderIds = new Set(orders.map((order) => order.id));
    const cases = must(await env.db.meal_cases.find({ owner_id })).filter(
      (entry) => orderIds.has(entry.order_id),
    );
    const profile = must(await env.db.meal_profiles.get({ owner_id }));
    const addresses = must(
      await env.db.meal_addresses.find({ owner_id, market }),
    );
    const privacyRequests = must(
      await env.db.meal_privacy_requests
        .find({ owner_id })
        .sort({ created_at: -1 }),
    );
    return {
      orders: orders.map(wire),
      plan,
      cases,
      profile,
      addresses,
      privacyRequests,
    };
  },
  { id: "gather.account", input: z.object({ market: marketSchema }) },
);
export const getOrder = query(
  async ({ id }: { id: string }) => {
    const row = await owned(id);
    await transact((tx) => expireReservations(tx, row.market));
    return wire(await owned(id));
  },
  { id: "gather.order", input: z.object({ id: z.string() }) },
);
export const payOrder = mutation(
  async ({ id, quote }: { id: string; quote?: Quote }) => {
    demo();
    const owner_id = user().id;
    return transact(async (tx) => {
      let row = await tx.meal_orders.get({ id, owner_id });
      if (!row) fail(/* i18n */ "Order not found.", "NOT_FOUND", 404);
      await expireReservations(tx, row.market);
      row = (await tx.meal_orders.get(id))!;
      if (row.payment === "succeeded") return wire(row);
      const order = wire(row);
      checkEditable(order);
      if (row.payment === "processing" || row.status === "checkout_expired")
        fail(
          /* i18n */ "We're checking your payment. Please wait for an update before trying again.",
          "PAYMENT_PENDING",
          409,
        );
      if (row.payment === "failed") {
        await requirePaymentReady(tx, owner_id, row.market);
        if (!quote)
          fail(
            /* i18n */ "Review the current total before trying payment again.",
            "QUOTE_REQUIRED",
            409,
          );
        const menu = await sellableMenu(
          tx,
          order.market,
          order.snapshot.cart.deliveryDate,
        );
        const authoritative = await trustedQuote(
          tx,
          owner_id,
          order.snapshot.cart,
          quote,
          menu,
        );
        row = changed(
          await tx.meal_orders.update(
            { id, version: row.version },
            {
              total: authoritative.total,
              snapshot: {
                ...order.snapshot,
                quote: authoritative,
                cutoff: menu.closesAt,
                recipes: menu.recipes.filter((r) =>
                  order.snapshot.cart.recipeIds.includes(r.id),
                ),
                consentAt: new Date().toISOString(),
              },
            },
          ),
        );
        row = await startAttempt(tx, row, "processing");
      }
      const attemptId = (row.snapshot as OrderSnapshot).paymentAttemptId;
      if (!attemptId)
        fail(
          /* i18n */ "Payment not found. Refresh the order and try again.",
          "NOT_FOUND",
          404,
        );
      return wire(await settleAttempt(tx, attemptId, "succeeded"));
    });
  },
  {
    id: "gather.pay",
    input: z.object({ id: z.string(), quote: quoteInputSchema.optional() }),
  },
);
export const cancelOrder = mutation(
  async ({ id }: { id: string }) => {
    demo();
    const row = await owned(id);
    const order = wire(row);
    if (order.status === "canceled") return order;
    checkEditable(order);
    return transact(async (tx) => {
      if (order.payment === "succeeded" && order.status === "confirmed")
        await adjustStock(tx, order.snapshot.cart, "release");
      else await releasePendingAttempt(tx, row);
      const updated = await tx.meal_orders.update(
        { id, version: row.version },
        {
          status: "canceled",
          refunded: order.payment === "succeeded" ? order.total : 0,
          timeline: [
            ...order.timeline,
            event(
              "canceled",
              order.payment === "succeeded"
                ? /* i18n */ "Box canceled and payment refunded"
                : /* i18n */ "Box canceled",
            ),
          ],
        },
      );
      if (!updated)
        fail(
          /* i18n */ "The order changed. Reload and try again.",
          "CONFLICT",
          409,
        );
      return wire(updated);
    });
  },
  { id: "gather.cancelOrder", input: z.object({ id: z.string() }) },
);
export const editOrder = mutation(
  async ({
    id,
    recipeIds,
    quote: requested,
  }: {
    id: string;
    recipeIds: string[];
    quote: Quote;
  }) => {
    demo();
    const row = await owned(id);
    const order = wire(row);
    checkEditable(order);
    if (order.payment !== "succeeded" || order.status !== "confirmed")
      fail(/* i18n */ "Resolve the payment before editing this box.");
    if (order.refunded > 0)
      fail(
        /* i18n */ "Contact support to change a box that has a refund.",
        "BOX_HAS_REFUND",
        409,
      );
    const cart = { ...order.snapshot.cart, recipeIds };
    return transact(async (tx) => {
      const menu = await sellableMenu(tx, cart.market, cart.deliveryDate);
      const quote = await trustedQuote(tx, row.owner_id, cart, requested, menu);
      await adjustStock(tx, order.snapshot.cart, "release");
      await expireReservations(tx, cart.market);
      await adjustStock(tx, cart, "reserve");
      const snapshot = {
        ...order.snapshot,
        cart,
        quote,
        recipes: menu.recipes.filter((r) => recipeIds.includes(r.id)),
      };
      const updated = await tx.meal_orders.update(
        { id, version: row.version },
        {
          snapshot,
          total: quote.total,
          timeline: [
            ...order.timeline,
            event(
              "edited",
              /* i18n */ "Meals updated; price adjustment {amount, number} {currency}",
              {
                amount: (quote.total - order.total) / 100,
                currency: quote.currency,
              },
            ),
          ],
        },
      );
      if (!updated)
        fail(
          /* i18n */ "The order changed. Reload and try again.",
          "CONFLICT",
          409,
        );
      return wire(updated);
    });
  },
  {
    id: "gather.editOrder",
    input: z.object({
      id: z.string(),
      recipeIds: z.array(z.string()).max(4),
      quote: quoteInputSchema,
    }),
  },
);
export const updatePlan = mutation(
  async ({
    market,
    action,
    version,
  }: {
    market: MarketId;
    action: "pause" | "resume" | "cancel" | "skip";
    version: number;
  }) => {
    const owner_id = user().id;
    return transact(async (tx) => {
      const plan = await tx.meal_plans.get({ owner_id, market });
      if (!plan) fail(/* i18n */ "No recurring plan found.", "NOT_FOUND", 404);
      if (plan.version !== version)
        fail(/* i18n */ "Your plan changed. Please retry.", "CONFLICT", 409);
      return changed(
        await tx.meal_plans.update(
          { id: plan.id, version },
          planChange(plan, action, market),
        ),
      );
    });
  },
  {
    id: "gather.plan",
    input: z.object({
      market: marketSchema,
      version: z.number().int().positive(),
      action: z.enum(["pause", "resume", "cancel", "skip"]),
    }),
  },
);
export const previewRenewal = query(
  async ({ market }: { market: MarketId }) => {
    const plan = must(
      await env.db.meal_plans.get({ owner_id: user().id, market }),
    );
    if (!plan || plan.status !== "active")
      fail(/* i18n */ "An active plan is required.", "PLAN_INACTIVE", 409);
    const configuration = plan.configuration as {
      cart: Cart;
      address: z.infer<typeof addressSchema>;
    };
    const cart = { ...configuration.cart, deliveryDate: plan.next_date };
    const menu = await transact(async (tx) =>
      sellableMenu(tx, cart.market, cart.deliveryDate),
    );
    return {
      date: plan.next_date,
      quote: quoteCart(cart, menu),
      cart,
      address: configuration.address,
      recipes: cart.recipeIds.map(
        (id) => menu.recipes.find((recipe) => recipe.id === id)!,
      ),
      market: cart.market,
    };
  },
  { id: "gather.renewalPreview", input: z.object({ market: marketSchema }) },
);

export const renewPlan = mutation(
  async ({
    market,
    date,
    total,
  }: {
    market: MarketId;
    date: string;
    total: number;
  }) => {
    demo();
    const owner_id = user().id;
    return transact(async (tx) => {
      const plan = await tx.meal_plans.get({ owner_id, market });
      if (!plan) fail(/* i18n */ "No recurring plan found.", "NOT_FOUND", 404);
      const request_key = `renewal:${plan.id}:${date}`;
      const existing = await tx.meal_orders.get({ request_key });
      if (existing) return wire(existing);
      if (plan.status !== "active" || plan.next_date !== date)
        fail(
          /* i18n */ "Your plan changed. Review the next delivery again.",
          "PLAN_CHANGED",
          409,
        );
      const configuration = plan.configuration as {
        cart: Cart;
        address: z.infer<typeof addressSchema>;
        consentAt?: string;
      };
      const cart = { ...configuration.cart, deliveryDate: date };
      const menu = await sellableMenu(tx, cart.market, cart.deliveryDate);
      const quote = quoteCart(cart, menu);
      if (quote.total !== total)
        fail(
          /* i18n */ "The total changed. Review the next delivery again.",
          "QUOTE_CHANGED",
          409,
        );
      await adjustStock(tx, cart, "reserve");
      const snapshot: OrderSnapshot = {
        cart,
        address: configuration.address,
        quote,
        recipes: menu.recipes.filter((r) => cart.recipeIds.includes(r.id)),
        cutoff: menu.closesAt,
        policyVersion: countryPolicies[cart.market].version,
        consentAt: new Date().toISOString(),
      };
      const order = await tx.meal_orders.insert({
        owner_id,
        market: cart.market,
        request_key,
        status: "confirmed",
        payment: "succeeded",
        fulfillment: "unallocated",
        total,
        refunded: 0,
        snapshot,
        timeline: [
          event("created", /* i18n */ "Recurring box created"),
          event(
            "payment_succeeded",
            /* i18n */ "Payment confirmed for your next box",
          ),
        ],
      });
      const next = new Date(`${date}T12:00:00Z`);
      next.setUTCDate(next.getUTCDate() + 7);
      const updated = await tx.meal_plans.update(
        { id: plan.id, version: plan.version },
        { next_date: next.toISOString().slice(0, 10) },
      );
      if (!updated)
        fail(/* i18n */ "Your plan changed. Please retry.", "CONFLICT", 409);
      return wire(order);
    });
  },
  {
    id: "gather.renewal",
    input: z.object({
      market: marketSchema,
      date: z.string().regex(/^\d{4}-\d{2}-\d{2}$/),
      total: z.number().int().nonnegative(),
    }),
  },
);
export const reportIssue = mutation(
  async ({
    orderId,
    category,
    message,
  }: {
    orderId: string;
    category: string;
    message: string;
  }) => {
    const row = await owned(orderId);
    return must(
      await env.db.meal_cases.insert({
        owner_id: row.owner_id,
        order_id: row.id as Id<"meal_orders">,
        category,
        message,
        status: "open",
      }),
    );
  },
  {
    id: "gather.issue",
    input: z.object({
      orderId: z.string(),
      category: z.enum(["missing", "quality", "delivery", "payment"]),
      message: z.string().trim().min(10).max(2000),
    }),
  },
);


export {
  saveAddress,
  deleteAddress,
  savePreferences,
  requestPrivacy,
  cancelPrivacyRequest,
  downloadPrivacyExport,
} from "./server/account";
export { getRecipe } from "./server/catalog";
export { getCookingRecipe, saveRecipeFeedback } from "./server/cooking";

// The saved-box endpoints the browser posts to directly.
export default {
  fetch: (request: Request) =>
    new URL(request.url).pathname === "/api/draft-session"
      ? draftSessionFetch(request)
      : draftFetch(request),
};
