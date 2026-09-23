"use server";

import { env } from "zeroship";
import { mutation } from "@zeroship/rpc/server";
import { bucket } from "@zeroship/storage";
import { z } from "zod";
import { addressSchema, validateAddress, fail, marketSchema } from "@gather/meal-kit/domain";
import { preferencesSchema } from "@gather/meal-kit/account-domain";
import { must, user, transact, changed } from "@gather/meal-kit/server/core";

const saveAddressInput = z
  .object({
    id: z.string().optional(),
    version: z.number().int().positive().optional(),
    market: marketSchema,
    label: z.string().trim().min(1).max(60),
    address: addressSchema,
    isDefault: z.boolean(),
  })
  .refine((value) => Boolean(value.id) === Boolean(value.version));

export const saveAddress = mutation(
  async (input: z.infer<typeof saveAddressInput>) => {
    const owner_id = user().id;
    validateAddress(input.address, input.market);
    return transact(async (tx) => {
      const existing = input.id
        ? await tx.meal_addresses.get({
            id: input.id,
            owner_id,
            market: input.market,
          })
        : null;
      if (input.id && !existing)
        fail(/* i18n */ "Address not found.", "NOT_FOUND", 404);
      if (existing && existing.version !== input.version) changed(null);
      const addresses = await tx.meal_addresses.find({
        owner_id,
        market: input.market,
      });
      if (!existing && addresses.length >= 20)
        fail(/* i18n */ "Remove an unused address before adding another.");
      const is_default =
        input.isDefault ||
        addresses.length === 0 ||
        Boolean(existing?.is_default);
      if (is_default) {
        for (const address of addresses.filter(
          (row) => row.is_default && row.id !== input.id,
        ))
          changed(
            await tx.meal_addresses.update(
              { id: address.id, version: address.version },
              { is_default: false },
            ),
          );
      }
      const values = { label: input.label, address: input.address, is_default };
      return existing
        ? changed(
            await tx.meal_addresses.update(
              { id: existing.id, version: input.version },
              values,
            ),
          )
        : tx.meal_addresses.insert({
            owner_id,
            market: input.market,
            ...values,
          });
    });
  },
  { id: "gather.saveAddress", input: saveAddressInput },
);

export const deleteAddress = mutation(
  async ({ id, version }: { id: string; version: number }) => {
    const owner_id = user().id;
    return transact(async (tx) => {
      const row = await tx.meal_addresses.get({ id, owner_id });
      if (!row) fail(/* i18n */ "Address not found.", "NOT_FOUND", 404);
      if (row.version !== version) changed(null);
      await tx.meal_addresses.delete({ id, owner_id, version });
      if (row.is_default) {
        const remaining = await tx.meal_addresses
          .find({ owner_id, market: row.market })
          .sort({ created_at: 1 });
        if (remaining[0])
          changed(
            await tx.meal_addresses.update(
              { id: remaining[0].id, version: remaining[0].version },
              { is_default: true },
            ),
          );
      }
      return { removed: true };
    });
  },
  {
    id: "gather.deleteAddress",
    input: z.object({ id: z.string(), version: z.number().int().positive() }),
  },
);

const preferenceInput = preferencesSchema.partial();
export const savePreferences = mutation(
  async (patch: z.infer<typeof preferenceInput>) => {
    const owner_id = user().id;
    return transact(async (tx) => {
      const existing = await tx.meal_profiles.get({ owner_id });
      const preferences = preferencesSchema.parse({
        ...preferencesSchema.parse(existing?.preferences ?? {}),
        ...patch,
      });
      if (patch.favorites)
        for (const slug of patch.favorites) {
          const recipe = await tx.meal_recipes.get({ slug, archived: false });
          if (!recipe)
            fail(/* i18n */ "This recipe is no longer available to save.");
        }
      return existing
        ? changed(
            await tx.meal_profiles.update(
              { id: existing.id, version: existing.version },
              { preferences },
            ),
          )
        : tx.meal_profiles.insert({ owner_id, preferences });
    });
  },
  { id: "gather.preferences", input: preferenceInput },
);

const privacyInput = z.object({
  kind: z.enum(["export", "deletion"]),
  requestKey: z.string().uuid(),
});
export const requestPrivacy = mutation(
  async ({ kind, requestKey }: z.infer<typeof privacyInput>) => {
    const owner_id = user().id;
    return transact(async (tx) => {
      const request_key = `${owner_id}:${requestKey}`;
      const existing = await tx.meal_privacy_requests.get({ request_key });
      if (existing) {
        if (existing.kind !== kind)
          fail(
            /* i18n */ "This request key belongs to another action.",
            "KEY_REUSED",
            409,
          );
        return existing;
      }
      const pending = await tx.meal_privacy_requests.get({
        owner_id,
        kind,
        status: "requested",
      });
      if (pending) return pending;
      return tx.meal_privacy_requests.insert({
        owner_id,
        request_key,
        kind,
        status: "requested",
        history: [
          {
            at: new Date().toISOString(),
            status: "requested",
            actor: owner_id,
          },
        ],
      });
    });
  },
  { id: "gather.requestPrivacy", input: privacyInput },
);

export const cancelPrivacyRequest = mutation(
  async ({ id }: { id: string }) => {
    const owner_id = user().id;
    return transact(async (tx) => {
      const request = await tx.meal_privacy_requests.get({ id, owner_id });
      if (!request)
        fail(/* i18n */ "Privacy request not found.", "NOT_FOUND", 404);
      if (request.status === "canceled") return request;
      if (request.status !== "requested")
        fail(
          /* i18n */ "This request is already being processed.",
          "REQUEST_LOCKED",
          409,
        );
      return changed(
        await tx.meal_privacy_requests.update(
          { id, version: request.version },
          {
            status: "canceled",
            history: [
              ...(request.history as {
                at: string;
                status: string;
                actor: string;
              }[]),
              {
                at: new Date().toISOString(),
                status: "canceled",
                actor: owner_id,
              },
            ],
          },
        ),
      );
    });
  },
  { id: "gather.cancelPrivacy", input: z.object({ id: z.string() }) },
);

export const downloadPrivacyExport = mutation(
  async ({ id }: { id: string }) => {
    const current = user();
    const owner_id = current.id;
    const request = must(
      await env.db.meal_privacy_requests.get({ id, owner_id }),
    );
    if (!request || request.kind !== "export")
      fail(/* i18n */ "Privacy request not found.", "NOT_FOUND", 404);
    if (!["requested", "completed"].includes(request.status))
      fail(
        /* i18n */ "This request is already being processed.",
        "REQUEST_LOCKED",
        409,
      );
    let key = request.object_key;
    const storage = bucket("gather-privacy");
    if (request.status !== "completed") {
      key = `${owner_id}/${request.id}/${crypto.randomUUID()}.json`;
      const document = await transact(async (tx) => ({
        identity: { id: owner_id, name: current.name, email: current.email },
        exportedAt: new Date().toISOString(),
        profile: await tx.meal_profiles.get({ owner_id }),
        addresses: await tx.meal_addresses.find({ owner_id }),
        boxes: (
          await tx.meal_carts.find({ principal: `user:${owner_id}` })
        ).map(({ market, cart, expires_at }) => ({
          market,
          cart,
          expiresAt: expires_at,
        })),
        plans: await tx.meal_plans.find({ owner_id }),
        orders: await tx.meal_orders.find({ owner_id }),
        support: await tx.meal_cases.find({ owner_id }),
        recipeFeedback: (await tx.meal_recipe_feedback.find({ owner_id })).map(
          ({ last_request_key, ...feedback }) => feedback,
        ),
        privacyRequests: await tx.meal_privacy_requests.find({ owner_id }),
      }));
      must(
        await storage.put(key, JSON.stringify(document, null, 2), {
          contentType: "application/json",
        }),
      );
      // Marking the request complete is a write, so it goes through
      // `transact` like every other write on this path. A write issued
      // outside a transaction runs on the session's autocommit connection,
      // which contends for the database's single writer lock with any
      // transaction another request holds; `transact` queues for the app's
      // transaction lane instead, so concurrent exports take their turn.
      await transact(async (tx) =>
        changed(
          await tx.meal_privacy_requests.update(
            { id, owner_id, version: request.version },
            {
              status: "completed",
              object_key: key,
              history: [
                ...(request.history as {
                  at: string;
                  status: string;
                  actor: string;
                }[]),
                {
                  at: new Date().toISOString(),
                  status: "completed",
                  actor: owner_id,
                },
              ],
            },
          ),
        ),
      );
    }
    if (!key)
      fail(
        /* i18n */ "Your export is unavailable. Please request a new export.",
        "EXPORT_UNAVAILABLE",
        503,
      );
    const object = must(await storage.get(key));
    if (!object)
      fail(
        /* i18n */ "Your export is unavailable. Please request a new export.",
        "EXPORT_UNAVAILABLE",
        503,
      );
    return { content: new TextDecoder().decode(object.bytes) };
  },
  { id: "gather.privacyExport", input: z.object({ id: z.string() }) },
);
