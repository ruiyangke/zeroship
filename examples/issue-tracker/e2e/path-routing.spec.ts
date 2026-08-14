import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * Routes are real paths, and they survive a reload.
 *
 * This app hand-rolled a router twice -- on the hash, then on the History API
 * -- and both shipped bugs a real router makes unspellable: a permalink
 * written as "#comment-3" replaced the route rather than jumping within the
 * page, and a link carrying "?attachment=" had its query swallowed into an id
 * because the parser split the whole URL on "/". It is react-router-dom now,
 * already this repo's router and pinned in the workspace catalog.
 *
 * The reload is the assertion that earns the change. A path router is only
 * correct if the server hands back the app for a path it has no file for --
 * vite in dev, and `"/[...rest]": { static: { try: ["$path", "/index.html"] } }`
 * in a built .zship. Get that wrong and every refresh on a deep link 404s,
 * which is the one failure the hash could never have.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("a deep path renders, survives reload, and has no hash", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Path ${RUN}`,
    key: productKey("PTH"),
    description: "path",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const issue = await rpc("issues.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Path routed ${RUN}`,
    description: "seed",
  });

  // Navigating in-app produces a real path, with nothing after a "#".
  await page.goto("/issues");
  await page.getByRole("link", { name: new RegExp(`Path routed ${RUN}`) }).first().click();
  await expect(page.locator("ul.comment-list")).toBeVisible();
  expect(page.url(), "the URL is a path").toContain(`/issues/${issue.id}`);
  expect(new URL(page.url()).hash, "and carries no fragment").toBe("");

  // The server serves the app for a path it has no file for.
  const direct = await page.request.get(`${baseURL}/issues/${issue.id}`);
  expect(direct.status(), "a deep path is served, not 404'd").toBe(200);
  expect(await direct.text(), "and it is the app's html").toContain("<div id=\"root\">");

  // A reload lands in the same place rather than on a 404 or the issue list.
  await page.reload();
  await expect(page.locator("ul.comment-list"), "the issue renders after a reload").toBeVisible();
  await expect(page.getByText("No page here"), "and it is not the not-found page").toHaveCount(0);
  expect(page.url()).toContain(`/issues/${issue.id}`);
});
