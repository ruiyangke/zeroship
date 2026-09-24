import { visit } from "./helpers";
import { chooseOption } from "./helpers";
import { testOrigin } from "../fixture/settings";
import { recipesForMarket } from "../../apps/backoffice/src/seed-catalog";
import { test, expect } from "./fixtures";
import { signIn, rpc, raw } from "./helpers";
import { defaultCart, type Order } from "@gather/meal-kit/domain";
import { cutoffForDate, money } from "@gather/meal-kit/catalog";
import type * as api from "../fixture/api";

test("China orders use a local menu and district address without a postcode throughout checkout and operations", async ({
  page,
  context,
  browser,
}) => {
  await signIn(page);
  await rpc(context, "saveAddress", {
    market: "cn",
    label: "Saved Shanghai address",
    isDefault: true,
    address: {
      country: "CN",
      province: "shanghai",
      city: "shanghai",
      district: "pudong",
      postal: "",
      name: "Alex Morgan",
      email: "alex@gather.example",
      line: "20 Previous Street",
      phone: "13800138000",
      instructions: "",
    },
  });
  // The same locator on the market that does ask for one, so the count of
  // zero on the China step below is a replaced field rather than a locator
  // that matches nothing anywhere.
  await visit(page, "/m/us/en/plans");
  await expect(page.getByLabel(/postal|postcode|ZIP/i)).toHaveCount(1);
  await visit(page, "/m/cn/en/plans");
  await expect(page.getByLabel(/postal|postcode|ZIP/i)).toHaveCount(0);
  await chooseOption(page.getByLabel("Province or municipality"), "Shanghai");
  await chooseOption(page.getByLabel("City", { exact: true }), "Shanghai");
  await chooseOption(page.getByLabel("District", { exact: true }), "Pudong");
  await page
    .getByRole("button", { name: "Continue to box size", exact: true })
    .click();
  await page
    .getByRole("button", { name: "See available meals", exact: true })
    .click();
  // China's own menu is on the page first: the absence of a US recipe below is
  // then a different menu, not a menu that has not rendered.
  await expect(
    page.getByRole("button", {
      name: `Add ${recipesForMarket("cn")[0].name}`,
      exact: true,
    }),
  ).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Garden pesto rigatoni" }),
  ).toHaveCount(0);
  for (const recipe of recipesForMarket("cn").slice(0, 3))
    await page
      .getByRole("button", { name: `Add ${recipe.name}`, exact: true })
      .click();
  let releaseAccount!: () => void;
  const accountHeld = new Promise<void>((resolve) => {
    releaseAccount = resolve;
  });
  await page.route(
    (url) => url.pathname.endsWith("/gather.account"),
    async (route) => {
      const response = await route.fetch();
      await accountHeld;
      await route.fulfill({ response });
    },
  );
  await page
    .getByRole("link", { name: "Review your box", exact: true })
    .click();
  await page.getByRole("link", { name: "Continue to checkout" }).click();
  // The delivery area this checkout did render is the control: the count of
  // zero below is a form without a postcode, not a form that never loaded.
  await expect(
    page.getByRole("combobox", { name: "District", exact: true }),
  ).toContainText("Pudong");
  await expect(page.getByLabel(/postal|postcode|ZIP/i)).toHaveCount(0);
  await page
    .getByLabel("Street, building and apartment")
    .fill("88 Garden Road, Building A, Apartment 501");
  await page.getByLabel("Mobile number").fill("12345");
  const loadedAccount = page.waitForResponse((response) =>
    new URL(response.url()).pathname.endsWith("/gather.account"),
  );
  releaseAccount();
  await loadedAccount;
  await expect(
    page.getByRole("group", { name: "Use a saved address" }),
  ).toBeVisible();
  await expect(page.getByLabel("Mobile number")).toHaveValue("12345");
  await expect(page.getByLabel("Street, building and apartment")).toHaveValue(
    "88 Garden Road, Building A, Apartment 501",
  );
  await page.getByRole("checkbox").check();
  await page.getByRole("button", { name: "Place order", exact: true }).click();
  await expect(page.getByRole("alert")).toHaveText(
    "Enter a valid phone number.",
  );
  await expect(page.getByLabel("Mobile number")).toBeFocused();
  await page.getByLabel("Mobile number").fill("13800138000");
  await chooseOption(page.getByLabel("Language", { exact: true }), /中文/);
  await expect(page.getByRole("combobox", { name: "区／县" })).toContainText(
    "浦东新区",
  );
  await expect(page.getByLabel("街道、小区、楼栋及门牌号")).toHaveValue(
    "88 Garden Road, Building A, Apartment 501",
  );
  await page.getByRole("checkbox").check();
  await page.getByRole("button", { name: "提交订单", exact: true }).click();
  await expect(page).toHaveURL(/\/m\/cn\/zh\/orders\//);
  const id = page.url().split("/").at(-1)!;
  const order = await rpc<Order>(context, "order", { id }, true);
  expect(order.snapshot.address).toMatchObject({
    country: "CN",
    postal: "",
    province: "shanghai",
    city: "shanghai",
    district: "pudong",
    phone: "13800138000",
  });
  expect(order.snapshot.quote.currency).toBe("CNY");
  expect(order.snapshot.cutoff).toBe(
    cutoffForDate("cn", order.snapshot.cart.deliveryDate),
  );
  await expect(page.getByText("上海 浦东新区", { exact: true })).toBeVisible();
  await chooseOption(page.getByLabel("语言", { exact: true }), "English");
  await page.getByRole("button", { name: "Edit meals", exact: true }).click();
  const edit = page.getByRole("dialog");
  await expect(edit.getByText(/^Updated total:/)).toHaveText(
    `Updated total: ${money(order.total, "cn", "en")}`,
  );
  await edit.getByRole("checkbox", { name: /Miso-glazed salmon/ }).uncheck();
  await edit.getByRole("checkbox", { name: /Sticky ginger tofu/ }).check();
  await expect(edit.getByText(/^Updated total:/)).toHaveText(
    `Updated total: ${money(order.total - 2000, "cn", "en")}`,
  );
  await edit
    .getByRole("button", { name: "Confirm meals and price", exact: true })
    .click();
  await expect(edit).not.toBeVisible();
  const edited = await rpc<Order>(context, "order", { id }, true);
  expect(edited.total).toBe(order.total - 2000);
  expect(edited.snapshot.cart.recipeIds).not.toContain("miso-salmon");
  const invalidCart = {
    ...defaultCart("cn"),
    area: order.snapshot.cart.area,
    recipeIds: ["lemon-chicken", "pesto-pasta", "miso-salmon"],
  };
  expect((await raw(context, "quote", invalidCart)).status()).toBe(400);
  expect(
    (
      await raw(context, "saveAddress", {
        market: "us",
        label: "Wrong country",
        address: order.snapshot.address,
        isDefault: false,
      })
    ).status(),
  ).toBe(400);
  await rpc(context, "saveAddress", {
    market: "cn",
    label: "Shanghai home",
    address: order.snapshot.address,
    isDefault: true,
  });
  const account = await rpc<Awaited<ReturnType<typeof api.getAccount>>>(
    context,
    "account",
    { market: "cn" },
    true,
  );
  expect(account.addresses.every((address) => address.market === "cn")).toBe(
    true,
  );
  const ops = await browser.newContext({ baseURL: testOrigin });
  const opsPage = await ops.newPage();
  await signIn(opsPage, "ops@gather.example");
  const operation = await rpc<Awaited<ReturnType<typeof api.getOperations>>>(
    ops,
    "operations",
    { market: "cn" },
    true,
  );
  expect(
    operation.orders.find((entry) => entry.id === id)?.snapshot.address
      .district,
  ).toBe("pudong");
  await rpc(ops, "prepareMenu", {
    market: "cn",
    date: order.snapshot.cart.deliveryDate,
  });
  const prepared = await rpc<Awaited<ReturnType<typeof api.getOperations>>>(
    ops,
    "operations",
    { market: "cn" },
    true,
  );
  expect(prepared.stock.some((row) => row.recipe_id === "pesto-pasta")).toBe(
    false,
  );
  await visit(opsPage, "/m/cn/zh/operations");
  await expect(
    opsPage.getByText("上海 浦东新区", { exact: true }).first(),
  ).toBeVisible();
  await ops.close();
});
