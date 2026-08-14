import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * The Details tab tells the story in the order it happened.
 *
 * `timeline.ts` opens by claiming exactly that -- "comments and field changes,
 * in the order they happened" -- and that claim is the whole argument for the
 * two-tab split: Details is the STORY, History is the LOG. Nothing tested it.
 * `history-timeline.spec.ts` drives the History tab and asserts who did what;
 * it never looks at the interleaving, which is the part a reader depends on.
 *
 * WHY THE WAITS ARE NOT PADDING. Activities are ordered by `changedAt` in
 * milliseconds. Fired back to back over RPC, a comment and a field change land
 * in the same millisecond and their relative order becomes arbitrary -- the
 * first version of this check drove five actions with no delay and saw the
 * event group render AFTER both comments, which reads exactly like a real
 * ordering bug and is not one. Do not remove the waits to speed this up; you
 * will get a test that fails a few percent of the time and blames the app.
 *
 * WHAT THIS DOES NOT CATCH: sub-second interleaving, for the reason above.
 * Two things a person does inside one millisecond have no defined order here
 * and this spec deliberately makes no claim about them. It also asserts only
 * on DOM position within the main column, so it would not notice the events
 * being styled invisibly.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

/** Long enough that `changedAt` differs; short enough not to pad the suite. */
const GAP_MS = 1200;
const nap = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

test("field changes sit between the comments they happened between", async ({
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
    name: `Story ${RUN}`,
    key: productKey("STO"),
    description: "story",
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
    summary: `Parser crashes on empty input ${RUN}`,
    description: "Steps: run it with an empty file.",
  });

  // The story, with real time between its beats.
  await rpc("comments.add", { issueId: issue.id, body: `FIRST reply ${RUN}` });
  await nap(GAP_MS);
  await rpc("issues.setPriority", { id: issue.id, priority: "P1" });
  await nap(GAP_MS);
  await rpc("comments.add", { issueId: issue.id, body: `SECOND reply ${RUN}` });

  await page.goto(`/issues/${issue.id}`);
  const main = page.locator(".issue-detail-main");
  await expect(main).toBeVisible();

  const first = main.getByText(`FIRST reply ${RUN}`);
  const second = main.getByText(`SECOND reply ${RUN}`);
  const event = main.getByText(/set Priority to P1/);

  await expect(first, "the first reply rendered").toBeVisible();
  await expect(second, "the second reply rendered").toBeVisible();
  await expect(
    event,
    "the priority change appears in the CONVERSATION, not only in the History tab -- this is the interleaving timeline.ts exists for",
  ).toBeVisible();

  // Document position, which is what a reader actually experiences. Comparing
  // bounding boxes would break the moment anything is laid out in a column
  // other than the obvious one.
  const orderOf = async (locator: typeof first) =>
    locator.evaluate((el) => {
      const nodes = [...document.querySelectorAll(".issue-detail-main *")];
      return nodes.indexOf(el);
    });

  const [a, e, b] = [await orderOf(first), await orderOf(event), await orderOf(second)];
  expect(a, "all three were found in the main column").toBeGreaterThan(-1);
  expect(
    e,
    "the change is AFTER the reply that preceded it in time",
  ).toBeGreaterThan(a);
  expect(
    b,
    "and BEFORE the reply that followed it -- an event parked at the end of the thread is the bug this guards",
  ).toBeGreaterThan(e);
});
