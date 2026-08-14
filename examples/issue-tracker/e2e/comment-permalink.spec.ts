import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * The "#3" beside a comment is a link you can follow and paste.
 *
 * It was `href="#comment-3"`. That is an anchor in an ordinary page, but this
 * app routes on the hash, so following it did not jump within the issue -- it
 * REPLACED the route, and every comment in every thread led to "No page here.
 * Nothing is routed at #comment-3."
 *
 * The same class of mistake sat on the attachment chip inside a comment:
 * `#/issues/<id>?attachment=<fileId>`. parseHash splits the hash on "/", so the
 * query rode along inside the id and the page asked the server for an issue
 * called "issu_0346...?attachment=atta_...". Nothing read the parameter. That
 * one is a download button now -- attachments come back over RPC as base64,
 * so there is no URL to point an anchor at.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("a comment permalink lands on the issue, at that comment", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Link ${RUN}`,
    key: productKey("LNK"),
    description: "link",
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
    summary: `Permalink ${RUN}`,
    description: "seed",
  });
  for (let i = 1; i <= 6; i++) {
    await rpc("comments.add", { issueId: issue.id, body: `Comment body number ${i}.` });
  }

  await page.setViewportSize({ width: 1440, height: 800 });
  await page.goto(`/issues/${issue.id}`);
  await expect(page.locator("ul.comment-list")).toBeVisible();

  // FOLLOW it, as a person does.
  await page.getByRole("link", { name: "#3", exact: true }).click();
  await page.waitForTimeout(800);

  // Still on the issue. This is the whole defect: it used to leave.
  await expect(
    page.getByText("No page here"),
    "following a comment link does not leave the issue for a dead route",
  ).toHaveCount(0);
  await expect(page.locator("ul.comment-list"), "the thread is still rendered").toBeVisible();

  // And the URL is one you could paste to someone.
  expect(page.url(), "the permalink is a route, not a bare fragment").toContain(
    `/issues/${issue.id}/c/3`,
  );

  // Pasting it cold lands in the same place.
  await page.goto(`/issues/${issue.id}/c/3`);
  await expect(page.locator("ul.comment-list")).toBeVisible();
  await expect(
    page.getByText("No page here"),
    "the pasted permalink resolves to the issue",
  ).toHaveCount(0);
  await expect(
    page.locator("li#comment-3"),
    "and the comment it names is on the page",
  ).toBeVisible();
});
