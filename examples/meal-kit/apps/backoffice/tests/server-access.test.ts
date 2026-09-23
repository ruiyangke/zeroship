// `packages/shared/src/server/core.ts` and `staff-access.ts` - the platform
// seams and the staff authorization both apps run through.
//
// Authorization is the part a shared database makes load-bearing: binding one
// database gives the storefront's process the same rows the back office
// writes, so what separates a customer from an operator is these checks and
// nothing else. `env` is the mutable test-time shim `packages/zeroship-stub`
// ships for exactly this; each test restores it.

import { afterEach, describe, expect, test } from "vitest";
import { env } from "zeroship";
import { changed, demo, must, transact, user } from "@gather/meal-kit/server/core";
import {
  administratorIds,
  forbidden,
  requireAdministrator,
  requirePermission,
  requireRecipeEditor,
  staffAccess,
} from "@gather/meal-kit/server/staff-access";
import { feedbackDto } from "@gather/meal-kit/server/feedback";
import type { StaffSettings } from "@gather/meal-kit/staff-domain";
import { memoryTx, refusal } from "./fixtures/tx";

const scope = env as Record<string, unknown>;
const signedInAs = (id: string | null) => {
  scope.auth = { getUser: () => (id ? { id, name: "Alex", email: "alex@gather.example" } : null) };
};
afterEach(() => {
  for (const key of ["auth", "db", "GATHER_ADMIN_IDS", "GATHER_MODE"]) delete scope[key];
});

const settings = (over: Partial<StaffSettings> = {}): StaffSettings => ({
  name: "Alex",
  active: true,
  recipeEditor: false,
  grants: [{ market: "us", roles: ["fulfillment"] }],
  ...over,
});

describe("the platform seams every server module shares", () => {
  test("must surfaces the engine's own error rather than a null row", () => {
    expect(must({ data: { id: "row" } })).toEqual({ id: "row" });
    const engine = new Error("connection reset");
    expect(() => must({ data: null, error: engine })).toThrow(engine);
  });

  test("a write that matched no row is a conflict the customer can act on", async () => {
    expect(changed({ id: "row" })).toEqual({ id: "row" });
    expect(await refusal(() => changed(null))).toMatchObject({
      code: "CONFLICT",
      status: 409,
    });
  });

  test("signing in is required before anything else, and the signed-in actor comes back whole", async () => {
    signedInAs(null);
    expect(await refusal(() => user())).toMatchObject({ code: "UNAUTHENTICATED", status: 401 });
    signedInAs("pws_gathercustomer000001");
    expect(user()).toMatchObject({ id: "pws_gathercustomer000001" });
  });

  test("the simulated payment surface is closed unless this deployment is the demo", async () => {
    demo();
    scope.GATHER_MODE = "demo";
    demo();
    scope.GATHER_MODE = "live";
    expect(await refusal(() => demo())).toMatchObject({ code: "PROVIDER_UNAVAILABLE", status: 503 });
  });

  test("a transaction runs at serializable isolation and hands the engine's error back", async () => {
    const seen: unknown[] = [];
    const handle = memoryTx().tx;
    scope.db = {
      transaction: async (fn: (tx: unknown) => Promise<unknown>, options: unknown) => {
        seen.push(options);
        return { data: await fn(handle) };
      },
    };
    expect(await transact(async (tx) => tx === handle)).toBe(true);
    expect(seen).toEqual([{ isolationLevel: "serializable" }]);
    const engine = new Error("serialization failure");
    scope.db = { transaction: async () => ({ data: null, error: engine }) };
    await expect(transact(async () => "unreached")).rejects.toThrow(engine);
  });
});

describe("who counts as staff", () => {
  test("the administrator list is read from configuration, trimmed, and empty when unset", () => {
    expect(administratorIds()).toEqual([]);
    scope.GATHER_ADMIN_IDS = " pws_one , ,pws_two ";
    expect(administratorIds()).toEqual(["pws_one", "pws_two"]);
  });

  test("an administrator holds every permission without a row, and a stranger holds none", async () => {
    scope.GATHER_ADMIN_IDS = "pws_admin";
    const db = memoryTx();
    expect(await staffAccess("pws_admin", db.tx)).toEqual({ administrator: true, recipeEditor: true, grants: [] });
    // The control: the table was never consulted for the administrator, and
    // someone absent from it gets nothing.
    expect(db.table("meal_staff_members").rows).toEqual([]);
    expect(await staffAccess("pws_stranger", db.tx)).toBeNull();
    signedInAs("pws_admin");
    expect(requireAdministrator()).toMatchObject({ id: "pws_admin" });
    signedInAs("pws_stranger");
    expect(await refusal(() => requireAdministrator())).toMatchObject({ code: "FORBIDDEN", status: 403 });
  });

  test("a stored member's grants are read through the transaction, and a deactivated one keeps none", async () => {
    const db = memoryTx();
    db.table("meal_staff_members").seed({ subject: "pws_colleague", settings: settings() });
    expect(await staffAccess("pws_colleague", db.tx)).toEqual({
      administrator: false,
      recipeEditor: false,
      grants: [{ market: "us", roles: ["fulfillment"] }],
    });
    db.table("meal_staff_members").rows[0].settings = settings({ active: false });
    expect(await staffAccess("pws_colleague", db.tx)).toBeNull();
  });

  test("without a transaction the same lookup goes through the app's own database binding", async () => {
    const asked: unknown[] = [];
    scope.db = {
      meal_staff_members: {
        get: async (filter: unknown) => {
          asked.push(filter);
          return { data: { subject: "pws_colleague", settings: settings({ recipeEditor: true }) } };
        },
      },
    };
    expect(await staffAccess("pws_colleague")).toMatchObject({ recipeEditor: true });
    expect(asked).toEqual([{ subject: "pws_colleague" }]);
  });

  test("a permission is granted for the country it was granted in and refused everywhere else", async () => {
    const db = memoryTx();
    db.table("meal_staff_members").seed({ subject: "pws_colleague", settings: settings() });
    signedInAs("pws_colleague");
    expect(await requirePermission("fulfillment", "us", db.tx)).toMatchObject({ id: "pws_colleague" });
    for (const [permission, market] of [
      ["fulfillment", "cn"],
      ["refund", "us"],
      ["catalog", "us"],
    ] as const)
      expect(
        await refusal(() => requirePermission(permission, market, db.tx)),
        `${permission} in ${market}`,
      ).toMatchObject({ code: "FORBIDDEN", status: 403 });
  });

  test("recipe editing is its own grant, independent of any country", async () => {
    const db = memoryTx();
    const member = db.table("meal_staff_members").seed({ subject: "pws_colleague", settings: settings() });
    signedInAs("pws_colleague");
    expect(await refusal(() => requireRecipeEditor(db.tx))).toMatchObject({ code: "FORBIDDEN", status: 403 });
    member.settings = settings({ recipeEditor: true, grants: [] });
    expect(await requireRecipeEditor(db.tx)).toMatchObject({ id: "pws_colleague" });
    // ...and it does not carry a country permission with it.
    expect(await refusal(() => requirePermission("fulfillment", "us", db.tx))).toMatchObject({ code: "FORBIDDEN" });
  });

  test("forbidden always refuses and never returns", async () => {
    expect(await refusal(() => forbidden())).toMatchObject({ code: "FORBIDDEN", status: 403 });
  });
});

describe("the review shape both apps read", () => {
  test("a stored review reaches the customer and the staff reader as one projection", () => {
    const row = {
      id: "fbk_1", version: 2, rating: 4, cook_again: true,
      comment: "The lemon chicken was excellent.",
      created_at: "2026-09-11T12:00:00.000Z", updated_at: "2026-09-12T09:30:00.000Z",
      owner_id: "pws_gathercustomer000001", last_request_key: "req-1",
    };
    expect(feedbackDto(row as never)).toEqual({
      id: "fbk_1", version: 2, rating: 4, cookAgain: true,
      comment: "The lemon chicken was excellent.",
      createdAt: "2026-09-11T12:00:00.000Z", updatedAt: "2026-09-12T09:30:00.000Z",
    });
    // An unanswered "would you cook it again" stays absent rather than false,
    // and the private columns beside it never leave the server.
    const quiet = feedbackDto({ ...row, cook_again: null } as never);
    expect(quiet.cookAgain).toBeNull();
    expect(Object.keys(quiet)).not.toContain("owner_id");
    expect(Object.keys(quiet)).not.toContain("last_request_key");
  });
});
