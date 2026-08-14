import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { otherUser, signIn } from "./session";

/**
 * The history reads as a timeline, and says who did each thing.
 *
 * It was a four-column table -- When / Field / Old / New -- with no actor
 * column at all, even though every activity row records an `actorId`. The
 * server's projection dropped it, so an audit trail could not name who acted:
 * the one question a history is usually opened to answer.
 *
 * Two people act here on purpose. With a single actor, "the timeline shows a
 * name" passes against a panel that hardcodes the signed-in user; only a
 * change made by someone else proves it reads the log.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("the history names who changed what, grouped per edit", async ({
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
    name: `Timeline ${RUN}`,
    key: productKey("TML"),
    description: "timeline",
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
    summary: `Timeline ${RUN}`,
    description: "seed",
  });

  // Bob makes the change, so the actor cannot be inferred from the viewer.
  const bobCtx = await browser.newContext();
  await signIn(bobCtx, { runtimePort: RUNTIME_PORT, baseURL: baseURL!, user: otherUser });
  const bob = await bobCtx.newPage();
  await bob.request.post(`${baseURL}/__zeroship/v1/issues.setSeverity`, {
    data: { json: { id: issue.id, severity: "major" } },
  });

  await page.goto(`/issues/${issue.id}`);
  await page.getByRole("button", { name: /^History/ }).click();

  const events = page.locator(".history-event");
  await expect(events.first(), "the log renders as timeline events").toBeVisible();

  // Bob's change names Bob, not the viewer.
  const bobEvent = events.filter({ hasText: otherUser.name });
  await expect(bobEvent, "the actor on the change is the person who made it").toHaveCount(1);
  await expect(bobEvent, "with the field and both values").toContainText("Severity");
  await expect(bobEvent).toContainText("normal");
  await expect(bobEvent).toContainText("major");

  // Alice filed the issue, so she owns the creation event.
  await expect(
    events.filter({ hasText: "Alice Dev" }).first(),
    "and the creation is attributed too",
  ).toBeVisible();

  // Field names are labels, not property names: "reporterId" leaked through
  // the generic splitter as "Reporter Id" until it was mapped.
  await expect(page.locator(".history-timeline")).not.toContainText("Reporter Id");

  // Creation writes one row per field. It renders as ONE event, folded, so
  // the real changes are not buried under fourteen "unset to" lines.
  const creation = events.filter({ hasText: "Alice Dev" }).first();
  const fold = creation.locator("details.history-creation");
  await expect(fold, "creation is folded rather than listed in full").toHaveCount(1);
  await expect(fold.locator("summary")).toContainText(/\d+ fields set on creation/);
  await expect(
    creation.locator(".history-changes > li").nth(3),
    "and its rows are not on screen while folded",
  ).toBeHidden();

  // Folded, NOT dropped. Opening it shows every row the server recorded --
  // this is the assertion that keeps the fold honest, because hiding history
  // to tidy a page would be a worse bug than the one being fixed.
  await fold.locator("summary").click();
  await expect(
    creation.locator(".history-changes > li").nth(3),
    "expanding reveals the rows, so nothing was hidden from the log",
  ).toBeVisible();
  await expect(creation).toContainText("Summary");

  await bobCtx.close();
});
