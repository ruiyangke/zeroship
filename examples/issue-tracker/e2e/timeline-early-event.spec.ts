import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * Something done seconds after filing is still something someone did.
 *
 * The timeline hides the creation event, because filing writes nineteen fields
 * at once and narrating them reads as nineteen changes nobody made. The rule
 * that found it was "no previous value, and within a second of the
 * description" -- which describes creation, but not only creation. A file
 * attached immediately after filing matches it exactly, and the upload
 * disappeared off the page with no trace that anything had been dropped.
 *
 * So this attaches over RPC rather than through the composer: the composer
 * needs a page load and some typing first, which puts the upload well outside
 * the window and lets the bug through. The narrow timing IS the test.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("a file attached moments after filing still appears in the timeline", async ({
  page,
  baseURL,
}) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Early ${RUN}`,
    key: productKey("ERL"),
    description: "early",
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
    summary: `Attached right away ${RUN}`,
    description: "seed",
  });

  // No page load in between: this lands within the same second as the
  // description, which is the case that used to vanish.
  await rpc("attachments.upload", {
    issueId: issue.id,
    filename: "early.log",
    contentBase64: Buffer.from("early\n").toString("base64"),
    contentType: "text/plain",
  });

  await page.goto(`/issues/${issue.id}`);

  const timeline = page.locator("ul.comment-list");
  await expect(timeline, "the thread rendered").toBeVisible();
  await expect(
    timeline.locator("li.timeline-event"),
    "the upload is narrated rather than swallowed as creation",
  ).toContainText("early.log");

  // And filing itself is still not narrated -- the fix must not have worked
  // by simply showing everything. Without this the spec passes on a build
  // that dropped the creation filter altogether.
  await expect(
    timeline.locator("li.timeline-event"),
    "creation stays out of the story",
  ).not.toContainText("set Summary");
});
