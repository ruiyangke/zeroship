import { visit } from "./helpers";
import { test, expect } from "./fixtures";
import { testOrigin } from "../fixture/settings";
import { boxSession, loadBox, seedBox, signIn, rpc } from "./helpers";
import { defaultCart } from "@gather/meal-kit/domain";
import { draftRevision, type DraftSave } from "@gather/meal-kit/draft-domain";

test("anonymous boxes use signed cookies and reject stolen proofs, stale edits and wrong owners", async ({
  page,
  context,
  browser,
}) => {
  const scope = await boxSession(context);
  const cookies = await context.cookies();
  const cookie = cookies.find((cookie) => cookie.name === "gather_draft")!;
  expect(cookie).toMatchObject({ httpOnly: true, sameSite: "Lax", path: "/" });
  const cart = {
    ...defaultCart("us"),
    postal: "10001",
    recipeIds: ["lemon-chicken"],
  };
  const input = {
    ...scope,
    cart,
    expected: null,
    requestKey: crypto.randomUUID(),
  };
  const create = await context.request.post("/api/drafts/save", {
    data: input,
  });
  expect(create.ok(), await create.text()).toBe(true);
  expect(create.headers()["cache-control"]).toContain("no-store");
  const first = (await create.json()) as DraftSave;
  expect(first.saved).toBe(true);
  const replay = await context.request.post("/api/drafts/save", {
    data: input,
  });
  expect((await replay.json()).state.draft).toEqual(first.state.draft);
  const other = await browser.newContext({ baseURL: testOrigin });
  try {
    await boxSession(other);
    expect(
      (await other.request.post("/api/drafts/load", { data: scope })).status(),
    ).toBe(403);
    expect(
      (
        await context.request.post("/api/drafts/load", {
          data: { ...scope, csrf: "forged" },
        })
      ).status(),
    ).toBe(403);
    expect(
      (
        await context.request.post("/api/drafts/save", {
          data: { ...input, owner: "another-customer" },
        })
      ).status(),
    ).toBe(409);
    expect(
      (
        await context.request.post("/api/drafts/save", {
          data: { ...input, market: "cn" },
        })
      ).status(),
    ).toBe(400);
    expect(
      (
        await context.request.post("/api/draft-session", {
          headers: {
            "X-Gather-Session": "init",
            "Sec-Fetch-Site": "cross-site",
          },
        })
      ).status(),
    ).toBe(403);
    expect((await context.request.get("/api/drafts/load")).status()).toBe(405);
    const expected = draftRevision(first.state.draft);
    const results = await Promise.all(
      [3, 4].map((servings) =>
        context.request.post("/api/drafts/save", {
          data: {
            ...input,
            cart: { ...cart, servings },
            expected,
            requestKey: crypto.randomUUID(),
          },
        }),
      ),
    );
    const outcomes = await Promise.all(
      results.map(async (response) => {
        expect(response.ok(), await response.text()).toBe(true);
        return response.json() as Promise<DraftSave>;
      }),
    );
    expect(outcomes.filter((result) => result.saved)).toHaveLength(1);
    const current = (await loadBox(context)).state.draft!;
    expect([3, 4]).toContain(current.cart.servings);
    await visit(page, "/m/us/en/box");
    await expect(page.locator(".box-slot.filled")).toHaveCount(1);
    const reopened = await context.newPage();
    await visit(reopened, "/m/us/en/box");
    await expect(reopened.locator(".box-slot.filled")).toHaveCount(1);
    expect(
      await reopened.evaluate(() => ({
        session: Object.keys(sessionStorage),
        local: Object.keys(localStorage),
      })),
    ).toEqual({ session: [], local: ["gather.market"] });
    await reopened.close();
  } finally {
    await other.close();
  }
});

test("a guest box attaches after sign-in, survives another device and stays private after sign-out", async ({
  page,
  context,
  browser,
}) => {
  const clean = await browser.newContext({ baseURL: testOrigin });
  try {
    await signIn(await clean.newPage(), "sam@gather.example");
    await seedBox(clean, {
      ...defaultCart("us"),
      postal: "10001",
      servings: 3,
      recipeIds: ["miso-salmon"],
    });
  } finally {
    await clean.close();
  }
  const cart = {
    ...defaultCart("us"),
    postal: "10001",
    servings: 5,
    recipeIds: ["lemon-chicken", "pesto-pasta"],
  };
  await seedBox(context, cart);
  await visit(page, "/m/us/en/box");
  await expect(page.locator(".box-slot.filled")).toHaveCount(2);
  const opened = page.waitForEvent("popup");
  await page.getByRole("button", { name: "Log in", exact: true }).click();
  const popup = await opened;
  await popup.getByLabel("Dev user").selectOption("sam@gather.example");
  await popup.getByRole("button", { name: "Sign in", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await expect(
    dialog.getByRole("heading", { name: "You have a saved box" }),
  ).toBeVisible();
  await dialog.getByRole("button", { name: "Keep this box" }).click();
  await expect(dialog).not.toBeVisible();
  await expect
    .poll(async () => (await loadBox(context)).state.draft?.cart.servings)
    .toBe(5);
  expect((await loadBox(context)).state.guest).toBeNull();
  const device = await browser.newContext({ baseURL: testOrigin });
  try {
    const otherPage = await device.newPage();
    await signIn(otherPage, "sam@gather.example", false);
    await visit(otherPage, "/m/us/en/box");
    await expect(otherPage.locator(".box-slot.filled")).toHaveCount(2);
    await seedBox(device, { ...cart, servings: 7 });
    await page
      .getByRole("link", { name: "Change delivery or box size" })
      .click();
    await page
      .getByRole("slider", { name: "People per meal" })
      .press("ArrowRight");
    await expect(
      dialog.getByRole("heading", { name: "Your box changed elsewhere" }),
    ).toBeVisible();
    await dialog.getByRole("button", { name: "Use saved box" }).click();
    await expect(
      page.getByRole("slider", { name: "People per meal" }),
    ).toHaveAttribute("aria-valuenow", "7");
  } finally {
    await device.close();
  }
  await page.getByRole("button", { name: "My account", exact: true }).click();
  await page.getByRole("menuitem", { name: "Sign out", exact: true }).click();
  await visit(page, "/m/us/en/box");
  // The box still renders its slots; the signed-out visitor just has none of
  // this customer's meals in them.
  await expect(page.locator(".box-slot")).toHaveCount(3);
  await expect(page.locator(".box-slot.filled")).toHaveCount(0);
});
