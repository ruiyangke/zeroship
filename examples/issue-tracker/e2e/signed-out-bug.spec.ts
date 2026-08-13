import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * A signed-out visitor can READ a bug and is not offered controls that fail.
 *
 * Bugs are public, so unlike the dashboard this page must not become a sign-in
 * wall. What it was doing instead was worse in a quieter way: it rendered a
 * comment composer, an Edit and a Make private button on every comment, and
 * three editable selects, every one of which answers 401 when used. The page
 * looked fully operable and was not.
 *
 * The pair below differs in ONE variable -- whether there is a session. The
 * signed-out half alone would pass against a page that shows nobody any
 * controls at all.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("a signed-out visitor reads the bug without being offered dead controls", async ({
  page,
  baseURL,
  browser,
}) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Anon ${RUN}`,
    key: productKey("ANO"),
    description: "anon",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const summary = `Readable while signed out ${RUN}`;
  const bug = await rpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary,
    description: "public description",
  });
  const secret = `private-${RUN}`;
  await rpc("comments.add", { bugId: bug.id, body: secret, isPrivate: true });

  // Signed IN -- the control half. Without it, "no composer" would pass
  // against a page that never offers one to anybody.
  await page.goto(`/#/bugs/${bug.id}`);
  await expect(page.getByLabel("Add a comment"), "a signed-in user can comment").toBeVisible();
  await expect(
    page.getByRole("combobox", { name: "Severity" }),
    "and can change fields",
  ).toBeEnabled();

  // Signed OUT.
  const anon = await browser.newContext();
  const visitor = await anon.newPage();
  await visitor.goto(`${baseURL}/#/bugs/${bug.id}`);

  await expect(visitor.locator("h1"), "the bug is still readable").toContainText(summary);
  await expect(visitor.getByText("public description")).toBeVisible();
  // Scoped to the banner: the title and its body both say it, so an unscoped
  // match is a strict-mode violation rather than a stronger assertion.
  await expect(
    visitor.getByText("You are not signed in"),
    "and the page says why it is read-only",
  ).toBeVisible();

  await expect(
    visitor.getByLabel("Add a comment"),
    "no composer is offered to someone who cannot post",
  ).toHaveCount(0);
  await expect(
    visitor.getByRole("button", { name: /^Make (comment )?private/ }),
    "nor per-comment actions",
  ).toHaveCount(0);
  await expect(
    visitor.getByRole("combobox", { name: "Severity" }),
    "and the field controls are disabled rather than merely doomed",
  ).toBeDisabled();

  // The private comment stays private -- this is the security half, and it
  // must hold regardless of how the controls are presented.
  await expect(visitor.locator("body")).not.toContainText(secret);

  await anon.close();
});
