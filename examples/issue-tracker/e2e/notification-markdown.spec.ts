import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { otherUser, signIn } from "./session";

/**
 * The inbox renders comment markdown; it does not print the source.
 *
 * Comment bodies are stored as markdown, and the notification body is a copy
 * of the comment that caused it. The dashboard printed that string straight
 * into a paragraph, so an ordinary comment arrived in the inbox as
 * "Reproduced on **staging**. The final `PUT` hangs" -- asterisks, backticks
 * and all. The storage format changed and this surface was never checked,
 * which is exactly the kind of gap a spec is for.
 *
 * Both halves are asserted: the emphasis renders as an element, AND the raw
 * markers are absent. Either alone would pass against a renderer that dropped
 * the body entirely.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("a notification renders its comment as markup, not as markdown source", async ({
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

  // Alice owns the bug so she is its reporter, which is what puts the comment
  // in her inbox. Bob writes it, because the fanout deliberately omits your
  // own changes.
  const product = await rpc("products.create", {
    name: `Inbox ${RUN}`,
    key: productKey("INB"),
    description: "inbox",
  });
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
    summary: `Inbox markdown ${RUN}`,
    description: "seed",
  });

  const bobCtx = await browser.newContext();
  await signIn(bobCtx, { runtimePort: RUNTIME_PORT, baseURL: baseURL!, user: otherUser });
  const bob = await bobCtx.newPage();
  const marker = `emphasised-${RUN}`;
  await bob.request.post(`${baseURL}/__zeroship/v1/comments.add`, {
    data: { json: { bugId: bug.id, body: `Reproduced on **${marker}** already.` } },
  });

  await page.goto("/#/dashboard");
  const row = page.locator("li", { hasText: marker }).first();
  await expect(row).toBeVisible({ timeout: 20_000 });

  await expect(
    row.locator("strong"),
    "the emphasis is rendered as markup",
  ).toHaveText(marker);
  await expect(
    row,
    "and the markdown source is not shown to the reader",
  ).not.toContainText("**");

  await bobCtx.close();
});
