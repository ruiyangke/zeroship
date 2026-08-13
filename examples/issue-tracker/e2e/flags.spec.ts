import { expect, test } from "@playwright/test";

import { signIn } from "./session";
import { chooseOption } from "./select";
import { productKey } from "./keys";

/**
 * Setting a flag on a bug, through the UI, end to end.
 *
 * This spec could not have been written before the change it guards. Every
 * `flags.*` procedure takes or returns a `flagTypeId`, and nothing in the app,
 * the migration or any seed could insert a row into `flagTypes` -- so the bug
 * page's Flags panel said "This product defines no bug-level flag types" for
 * every product that would ever exist. Four procedures, a panel and a section
 * of SPEC.md described a feature that could not be reached.
 *
 * The absence was invisible to everything else: the panel rendered its empty
 * state correctly, the procedures compiled and typechecked, and no test named
 * them because no test could reach them.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

test("an admin defines a flag type, and it becomes settable on a bug", async ({
  page,
  baseURL,
  context,
}) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", { key: productKey("FLAG"),
    name: `Flags ${RUN}`, description: "flags" });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const bug = await rpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Needs review ${RUN}`,
    description: "flag me",
  });

  // The control half, taken BEFORE the type exists. Without it, a panel that
  // was broken for some other reason would look the same as one correctly
  // reporting that no types are defined.
  const typeName = `review-${RUN}`.slice(0, 30);
  await page.goto(`/#/bugs/${bug.id}`);
  const flags = page.locator("section.flags-panel");
  await expect(flags).toBeVisible();
  // About THIS type, not "no types at all". A flag type with no product is
  // global and shows up on every bug, so an absolute assertion here passes
  // only against a virgin database -- it passed alone and failed in the suite,
  // decided by whether an earlier spec had left a global type behind.
  await expect(flags, "this type does not exist yet").not.toContainText(typeName);

  // Define one through the admin page, the way an operator would.
  await page.goto("/#/products");
  const admin = page.locator("section.flag-types-admin");
  await expect(admin).toBeVisible();
  await admin.getByLabel("New flag type").fill(typeName);
  // Scoped to this run's product. Left global it would appear on every bug in
  // the database, which is what made the first version of this spec order-
  // dependent.
  await chooseOption(page, admin, "Product", `Flags ${RUN}`);
  await admin.getByRole("button", { name: "Create" }).click();
  await expect(admin.locator("ul.flag-type-list")).toContainText(typeName);

  // The bug page now offers it. `products.get` already returned the product's
  // flag types, so nothing there needed changing -- the table was simply
  // always empty.
  await page.goto(`/#/bugs/${bug.id}`);
  await expect(flags).toBeVisible();
  await expect(flags, "the new type should be offered on the bug").toContainText(typeName);
  await expect(flags).not.toContainText(/defines no bug-level flag types/i);

  // And it must NOT have leaked onto an unrelated product.
  const other = await rpc("products.create", { key: productKey("UNRE"),
    name: `Unrelated ${RUN}`, description: "other" });
  const otherComponent = await rpc("components.create", {
    productId: other.id,
    name: "Core",
    description: "core",
  });
  const otherVersion = await rpc("versions.create", { productId: other.id, name: "1.0" });
  const otherBug = await rpc("bugs.create", {
    productId: other.id,
    componentId: otherComponent.id,
    versionId: otherVersion.id,
    summary: `Unrelated bug ${RUN}`,
    description: "no flags here",
  });
  await page.goto(`/#/bugs/${otherBug.id}`);
  await expect(flags, "a product-scoped type must not appear on another product").not.toContainText(
    typeName,
  );
});
