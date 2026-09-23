import { visit } from "./helpers";
import { chooseOption } from "./helpers";
import { testOrigin } from "../fixture/settings";
import AxeBuilder from "@axe-core/playwright";
import { test, expect } from "./fixtures";
import { signIn, rpc, raw } from "./helpers";
import { deliveryDates, deliveryLabel } from "@gather/meal-kit/catalog";
import { defaultCart, type Quote, type Order } from "@gather/meal-kit/domain";
import type { RecipeDraft, MenuDraft } from "@gather/meal-kit/catalog-domain";
import type * as api from "../fixture/api";
type Workspace = Awaited<ReturnType<typeof api.getCatalogWorkspace>>;
type SavedRecipe = Awaited<ReturnType<typeof api.saveRecipeDraft>>;

test("staff approve recipe versions and publish country menus without changing purchased recipes", async ({
  page,
  context,
  browser,
}) => {
  await signIn(page, "ops@gather.example");
  const initial = await rpc<Workspace>(
    context,
    "catalogWorkspace",
    { market: "cn" },
    true,
  );
  const source = initial.recipes.find(
    (recipe) => recipe.slug === "lemon-chicken",
  )!;
  const slug = "editorial-" + crypto.randomUUID();
  const draft = {
    ...(source.draft as RecipeDraft),
    name: "Market kitchen special",
    translations: {
      zh: {
        ...(source.draft as RecipeDraft).translations.zh,
        name: "本周厨房特选",
      },
    },
  };
  const created = await rpc<SavedRecipe>(context, "saveRecipeDraft", {
    slug,
    draft: {
      ...draft,
      name: "",
      translations: { zh: { ...draft.translations.zh, name: "" } },
    },
  });
  expect(
    (
      await raw(context, "approveRecipe", {
        id: created.id,
        version: created.version,
        note: "Must not approve an unfinished translation.",
      })
    ).status(),
  ).toBe(400);
  const customer = await browser.newContext({
    baseURL: testOrigin,
  });
  try {
    const customerPage = await customer.newPage();
    await signIn(customerPage);
    expect((await raw(customer, "recipe", { slug }, true)).status()).toBe(404);
    expect(
      (
        await raw(customer, "catalogWorkspace", { market: "cn" }, true)
      ).status(),
    ).toBe(404);
    expect(
      (
        await raw(customer, "approveRecipe", {
          id: created.id,
          version: created.version,
          note: "Unauthorized approval",
        })
      ).status(),
    ).toBe(404);
    await visit(page, "/m/cn/en/operations");
    await page
      .getByRole("tab", { name: "Menus & recipes", exact: true })
      .click();
    await page
      .getByRole("tab", { name: "Recipe library", exact: true })
      .click();
    await chooseOption(page.getByLabel("Recipe", { exact: true }), slug);
    await page
      .getByLabel("Recipe name", { exact: true })
      .fill("Market kitchen special reviewed");
    await chooseOption(page.getByLabel("Content language"), /中文/);
    await page.getByLabel("Recipe name", { exact: true }).fill("本周厨房特选");
    await page
      .getByRole("button", { name: "Save recipe draft", exact: true })
      .click();
    await expect(
      page.getByRole("status").filter({ hasText: "Recipe draft saved." }),
    ).toBeVisible();
    await page
      .getByRole("button", { name: "Approve saved recipe", exact: true })
      .click();
    const approval = page.getByRole("dialog");
    await approval
      .getByLabel("Review notes")
      .fill(
        "Reviewed ingredients, allergens and bilingual preparation instructions.",
      );
    await approval
      .getByRole("button", { name: "Confirm approval", exact: true })
      .click();
    await expect(approval).not.toBeVisible();
    expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
    await page.screenshot({
      path: "tests/.artifacts/recipe-editor-desktop.png",
      fullPage: true,
    });
    await page.setViewportSize({ width: 390, height: 844 });
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
    await page.screenshot({
      path: "tests/.artifacts/recipe-editor-mobile.png",
      fullPage: true,
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    let workspace = await rpc<Workspace>(
      context,
      "catalogWorkspace",
      { market: "cn" },
      true,
    );
    const version = workspace.versions.find(
      (version) => version.recipe_id === created.id,
    )!;
    expect(version).toMatchObject({
      revision: 1,
      approved_by: "pws_gatheroperator000001",
    });
    const date = deliveryDates("cn").at(-1)!;
    let menu = workspace.menus.find((menu) => menu.delivery_date === date)!;
    const menuDraft = {
      ...(menu.draft as MenuDraft),
      offerings: [
        ...(menu.draft as MenuDraft).offerings.slice(0, 2),
        { recipeVersionId: version.id, premium: 150 },
      ],
    };
    menu = await rpc(context, "saveMenuDraft", {
      market: "cn",
      date,
      id: menu.id,
      version: menu.version,
      draft: menuDraft,
    });
    let catalog = await rpc<Awaited<ReturnType<typeof api.getCatalog>>>(
      customer,
      "catalog",
      { market: "cn", date },
      true,
    );
    expect(catalog.recipes.some((recipe) => recipe.id === slug)).toBe(false);
    await page.reload();
    await page
      .getByRole("tab", { name: "Menus & recipes", exact: true })
      .click();
    await chooseOption(
      page.getByLabel("Menu delivery date"),
      deliveryLabel(date, "cn", "en"),
    );
    await page
      .getByRole("button", { name: "Publish saved menu", exact: true })
      .click();
    await expect(
      page.getByRole("status").filter({ hasText: "Menu published." }),
    ).toBeVisible();
    catalog = await rpc(customer, "catalog", { market: "cn", date }, true);
    expect(
      catalog.recipes.find((recipe) => recipe.id === slug)?.versionId,
    ).toBe(version.id);
    expect(
      catalog.availability.find((recipe) => recipe.recipeId === slug)
        ?.available,
    ).toBe(0);
    const cart = {
      ...defaultCart("cn"),
      deliveryDate: date,
      area: { province: "shanghai", city: "shanghai", district: "pudong" },
      recipeIds: catalog.recipes.map((recipe) => recipe.id),
      recurring: false,
    };
    const quote = await rpc<Quote>(customer, "quote", cart);
    const checkout = {
      cart,
      quote,
      address: {
        country: "CN",
        province: "shanghai",
        city: "shanghai",
        district: "pudong",
        name: "Alex Morgan",
        email: "",
        postal: "",
        line: "88 Garden Road, Apartment 501",
        phone: "13800138000",
        instructions: "",
      },
      requestKey: crypto.randomUUID(),
      consent: true,
    };
    expect((await raw(customer, "checkout", checkout)).status()).toBe(409);
    await rpc(context, "inventory", {
      stockKey: `cn:${date}:${slug}`,
      available: 8,
      published: true,
    });
    const order = await rpc<Order>(customer, "checkout", checkout);
    expect(
      order.snapshot.recipes.find((recipe) => recipe.id === slug)?.translations
        .zh.name,
    ).toBe("本周厨房特选");
    workspace = await rpc(context, "catalogWorkspace", { market: "cn" }, true);
    const current = workspace.recipes.find(
      (recipe) => recipe.id === created.id,
    )!;
    const changed = await rpc<SavedRecipe>(context, "saveRecipeDraft", {
      id: current.id,
      version: current.version,
      slug,
      draft: {
        ...(current.draft as RecipeDraft),
        name: "A future recipe",
        translations: {
          zh: {
            ...(current.draft as RecipeDraft).translations.zh,
            name: "下周新菜",
          },
        },
      },
    });
    const next = await rpc<Workspace["versions"][number]>(
      context,
      "approveRecipe",
      {
        id: changed.id,
        version: changed.version,
        note: "Reviewed the next menu version.",
      },
    );
    expect(next.revision).toBe(2);
    expect(
      (
        await raw(context, "saveRecipeDraft", {
          id: current.id,
          version: current.version,
          slug,
          draft,
        })
      ).status(),
    ).toBe(409);
    workspace = await rpc(context, "catalogWorkspace", { market: "cn" }, true);
    menu = workspace.menus.find((menu) => menu.delivery_date === date)!;
    menu = await rpc(context, "saveMenuDraft", {
      market: "cn",
      date,
      id: menu.id,
      version: menu.version,
      draft: {
        ...menuDraft,
        price: menuDraft.price + 100,
        offerings: menuDraft.offerings.map((offering) =>
          offering.recipeVersionId === version.id
            ? { ...offering, recipeVersionId: next.id }
            : offering,
        ),
      },
    });
    await rpc(context, "publishMenu", { id: menu.id, version: menu.version });
    expect(
      (
        await raw(customer, "checkout", {
          ...checkout,
          requestKey: crypto.randomUUID(),
        })
      ).status(),
    ).toBe(409);
    expect(
      (await rpc<Order>(customer, "order", { id: order.id }, true)).snapshot,
    ).toEqual(order.snapshot);
    await visit(customerPage, `/m/cn/zh/orders/${order.id}/cook/${slug}`);
    await expect(
      customerPage.getByRole("heading", { name: "本周厨房特选", exact: true }),
    ).toBeVisible();
    await expect(
      customerPage.getByRole("heading", { name: "下周新菜", exact: true }),
    ).toHaveCount(0);
    await expect(customerPage.getByRole("slider")).toHaveCount(0);
    await expect(
      customerPage.getByText("300 克", { exact: true }),
    ).toBeVisible();
    const us = await rpc<Awaited<ReturnType<typeof api.getCatalog>>>(
      customer,
      "catalog",
      { market: "us", date: deliveryDates("us")[0] },
      true,
    );
    expect(us.recipes.some((recipe) => recipe.id === slug)).toBe(false);
    workspace = await rpc(context, "catalogWorkspace", { market: "cn" }, true);
    menu = workspace.menus.find((menu) => menu.delivery_date === date)!;
    await rpc(context, "withdrawMenu", { id: menu.id, version: menu.version });
    expect(
      (
        await rpc<Awaited<ReturnType<typeof api.getCatalog>>>(
          customer,
          "catalog",
          { market: "cn", date },
          true,
        )
      ).recipes,
    ).toEqual([]);
    expect((await raw(customer, "quote", cart)).status()).toBe(409);
    expect(
      (await rpc<Order>(customer, "order", { id: order.id }, true)).snapshot,
    ).toEqual(order.snapshot);
  } finally {
    await customer.close();
  }
});
