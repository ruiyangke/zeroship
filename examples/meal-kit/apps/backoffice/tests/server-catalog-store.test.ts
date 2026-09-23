// `packages/shared/src/server/catalog-store.ts` - the menu the back office
// publishes and the storefront sells from.
//
// One database means the storefront reads exactly the row this publish writes,
// so the guards here are the only thing standing between a half-finished draft
// and a customer's basket.

import { describe, expect, test } from "vitest";
import {
  publishMenuVersion,
  publishedMenu,
  recipeVersion,
  sellableMenu,
} from "@gather/meal-kit/server/catalog-store";
import type { SaleMenu } from "@gather/meal-kit/catalog-domain";
import { cutoffForDate, deliveryDates, markets } from "@gather/meal-kit/catalog";
import { recipes, seedRecipeDraft } from "../src/seed-catalog";
import { memoryTx, refusal } from "./fixtures/tx";

const date = deliveryDates("us")[0];
const closesAt = cutoffForDate("us", date);
const opensAt = new Date(Date.now() - 86_400_000).toISOString();

type Db = ReturnType<typeof memoryTx>;

/** A recipe master plus one approved version of it, the way the back office stores them. */
function approved(db: Db, slug: string, over: Record<string, unknown> = {}) {
  const seed = recipes.find((recipe) => recipe.id === slug)!;
  const master = db.table("meal_recipes").seed({ slug, draft: seedRecipeDraft(seed, (text) => text), archived: false, ...over });
  const version = db.table("meal_recipe_versions").seed({
    recipe_id: master.id,
    revision: 1,
    content: seedRecipeDraft(seed, (text) => text),
  });
  return { master, version };
}

function drafted(db: Db, offerings: { recipeVersionId: string; premium: number }[]) {
  return db.table("meal_menus").seed({
    menu_key: `us:${date}`,
    market: "us",
    delivery_date: date,
    status: "draft",
    published_version_id: null,
    draft: { price: markets.us.price, shipping: markets.us.shipping, opensAt, closesAt, offerings },
    history: [],
  });
}

describe("reading a published menu", () => {
  test("only a published menu with a published version is sellable, and it carries the version's identity", async () => {
    const db = memoryTx();
    const content = { market: "us", date, revision: 3, price: 1000, shipping: 500, currency: markets.us.currency, opensAt, closesAt, recipes: [] };
    const version = db.table("meal_menu_versions").seed({ menu_id: "menu_1", market: "us", revision: 3, content });
    const menu = db.table("meal_menus").seed({ id: "menu_1", menu_key: `us:${date}`, market: "us", delivery_date: date, status: "published", published_version_id: version.id });
    const found = (await publishedMenu(db.tx, "us", date))!;
    expect(found).toMatchObject({ ...content, id: version.id });
    // Each control moves one thing: the status, then the pointer.
    menu.status = "draft";
    expect(await publishedMenu(db.tx, "us", date)).toBeNull();
    menu.status = "published";
    menu.published_version_id = null;
    expect(await publishedMenu(db.tx, "us", date)).toBeNull();
    // ...and a date nobody published is simply absent.
    menu.published_version_id = version.id;
    expect(await publishedMenu(db.tx, "us", "2026-01-01")).toBeNull();
  });

  test("a published pointer into another menu's version is corruption, not an empty menu", async () => {
    const db = memoryTx();
    const version = db.table("meal_menu_versions").seed({ menu_id: "menu_other", market: "us", revision: 1, content: {} });
    db.table("meal_menus").seed({ id: "menu_1", menu_key: `us:${date}`, market: "us", delivery_date: date, status: "published", published_version_id: version.id });
    await expect(publishedMenu(db.tx, "us", date)).rejects.toThrow("Published menu reference is invalid.");
  });

  test("a menu outside its sale window cannot be sold from, however published it is", async () => {
    const db = memoryTx();
    const content = (over: Partial<SaleMenu>) => ({
      market: "us", date, revision: 1, price: 1000, shipping: 500,
      currency: markets.us.currency, opensAt, closesAt, recipes: [], ...over,
    });
    const version = db.table("meal_menu_versions").seed({ menu_id: "menu_1", market: "us", revision: 1, content: content({}) });
    db.table("meal_menus").seed({ id: "menu_1", menu_key: `us:${date}`, market: "us", delivery_date: date, status: "published", published_version_id: version.id });
    expect(await sellableMenu(db.tx, "us", date)).toMatchObject({ id: version.id });
    for (const [label, over] of [
      ["it has not opened yet", { opensAt: new Date(Date.now() + 86_400_000).toISOString() }],
      ["it has closed", { closesAt: new Date(Date.now() - 1000).toISOString() }],
      ["it prices in another country's currency", { currency: markets.uk.currency }],
    ] as const) {
      version.content = content(over);
      expect(await refusal(() => sellableMenu(db.tx, "us", date)), label).toMatchObject({
        code: "MENU_UNAVAILABLE",
        status: 409,
      });
    }
  });

  test("an approved version resolves to its master's slug, and an unknown one is refused", async () => {
    const db = memoryTx();
    const { version } = approved(db, "lemon-chicken");
    expect(await recipeVersion(db.tx, version.id)).toMatchObject({
      id: "lemon-chicken",
      versionId: version.id,
      revision: 1,
      premium: 0,
    });
    expect(await refusal(() => recipeVersion(db.tx, "no-such-version"))).toMatchObject({
      code: "RECIPE_NOT_APPROVED",
    });
    // The control: a version whose master has gone is a different failure.
    db.table("meal_recipes").rows.length = 0;
    expect(await refusal(() => recipeVersion(db.tx, version.id))).toMatchObject({
      code: "NOT_FOUND",
      status: 404,
    });
  });
});

describe("publishing a menu", () => {
  test("publishing records a new revision, points the menu at it, logs who did it and opens stock for every meal plus delivery", async () => {
    const db = memoryTx();
    const chicken = approved(db, "lemon-chicken");
    const pasta = approved(db, "pesto-pasta");
    const menu = drafted(db, [
      { recipeVersionId: chicken.version.id, premium: 0 },
      { recipeVersionId: pasta.version.id, premium: 250 },
    ]);

    const published = await publishMenuVersion(db.tx, menu.id, menu.version, "pws_gatheroperator000001");
    expect(published).toMatchObject({ market: "us", date, revision: 1, price: markets.us.price });
    expect(published.recipes.map((recipe) => [recipe.id, recipe.premium])).toEqual([
      ["lemon-chicken", 0],
      ["pesto-pasta", 250],
    ]);
    const stored = db.table("meal_menus").rows[0];
    expect(stored.status).toBe("published");
    expect(stored.published_version_id).toBe(published.id);
    expect(stored.history).toEqual([
      expect.objectContaining({ action: "published", actor: "pws_gatheroperator000001", versionId: published.id }),
    ]);
    expect(db.table("meal_inventory").rows.map((row) => [row.stock_key, row.available, row.published])).toEqual([
      [`us:${date}:lemon-chicken`, 0, true],
      [`us:${date}:pesto-pasta`, 0, true],
      [`us:${date}:delivery`, 0, true],
    ]);

    // Publishing again takes the next revision and leaves the stock it already
    // opened alone rather than resetting a country's counts to zero.
    db.table("meal_inventory").rows[0].available = 40;
    const second = await publishMenuVersion(db.tx, menu.id, stored.version, "pws_gatheroperator000001");
    expect(second.revision).toBe(2);
    expect(db.table("meal_menu_versions").rows).toHaveLength(2);
    expect(db.table("meal_inventory").rows).toHaveLength(3);
    expect(db.table("meal_inventory").rows[0].available).toBe(40);
  });

  test("a menu the operator did not have in front of them is a conflict, and an unknown one is not found", async () => {
    const db = memoryTx();
    const chicken = approved(db, "lemon-chicken");
    const menu = drafted(db, [{ recipeVersionId: chicken.version.id, premium: 0 }]);
    expect(await refusal(() => publishMenuVersion(db.tx, menu.id, menu.version + 1, "pws_admin"))).toMatchObject({
      code: "CONFLICT",
      status: 409,
    });
    expect(await refusal(() => publishMenuVersion(db.tx, "no-such-menu", 1, "pws_admin"))).toMatchObject({
      code: "NOT_FOUND",
      status: 404,
    });
    // The control: unchanged, the same publish goes through.
    expect(await publishMenuVersion(db.tx, menu.id, menu.version, "pws_admin")).toMatchObject({ revision: 1 });
  });

  test("two approved versions of one recipe cannot both be on sale, and an archived recipe cannot be on sale at all", async () => {
    const duplicate = memoryTx();
    const first = approved(duplicate, "lemon-chicken");
    const second = duplicate.table("meal_recipe_versions").seed({
      recipe_id: first.master.id,
      revision: 2,
      content: first.version.content,
    });
    const both = drafted(duplicate, [
      { recipeVersionId: first.version.id, premium: 0 },
      { recipeVersionId: second.id, premium: 0 },
    ]);
    expect(await refusal(() => publishMenuVersion(duplicate.tx, both.id, both.version, "pws_admin"))).toMatchObject({
      message: "Choose each recipe only once.",
    });
    expect(duplicate.table("meal_menus").rows[0].status).toBe("draft");

    const archived = memoryTx();
    const gone = approved(archived, "lemon-chicken", { archived: true });
    const menu = drafted(archived, [{ recipeVersionId: gone.version.id, premium: 0 }]);
    expect(await refusal(() => publishMenuVersion(archived.tx, menu.id, menu.version, "pws_admin"))).toMatchObject({
      message: "Remove archived recipes before publishing.",
    });
    expect(archived.table("meal_menus").rows[0].status).toBe("draft");
    expect(archived.table("meal_inventory").rows).toEqual([]);
  });

  test("a draft with no meals in it, or a sale window running past the delivery cutoff, is refused before anything is written", async () => {
    for (const [label, draft, code] of [
      ["no offerings", { offerings: [] }, "EMPTY_MENU"],
      ["closes after the cutoff", { closesAt: new Date(Date.parse(closesAt) + 60_000).toISOString() }, "INVALID_SALE_WINDOW"],
      ["opens after it closes", { opensAt: new Date(Date.parse(closesAt) + 60_000).toISOString() }, "INVALID_SALE_WINDOW"],
    ] as const) {
      const db = memoryTx();
      const chicken = approved(db, "lemon-chicken");
      const menu = drafted(db, [{ recipeVersionId: chicken.version.id, premium: 0 }]);
      menu.draft = { ...(menu.draft as object), ...draft };
      expect(await refusal(() => publishMenuVersion(db.tx, menu.id, menu.version, "pws_admin")), label).toMatchObject({ code });
      expect(db.table("meal_menu_versions").rows, label).toEqual([]);
      expect(db.table("meal_inventory").rows, label).toEqual([]);
    }
  });
});
