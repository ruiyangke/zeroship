import { expect, test } from "@playwright/test";

import { otherUser, signIn } from "./session";
import { productKey } from "./keys";

/**
 * The two clusters nothing drove end to end: saved searches, and the unread
 * notification count.
 *
 * Both are backed by `env.kv` -- a saved search writes a cache entry keyed by
 * owner, and the unread count is cached and invalidated by `notifications.
 * markRead`. A stale or un-invalidated cache is invisible to a single read;
 * it shows up only when a value is read, changed, and read again, which is
 * what both tests below do.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

test("a saved search survives update and is gone after delete", async ({
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

  const saved = await rpc("savedSearches.save", {
    name: `search-${RUN}`,
    queryJson: { field: "status", operator: "eq", value: "CONFIRMED" },
  });
  const ids = (rows: { id: string }[]) => rows.map((row) => row.id);
  expect(ids(await rpc("savedSearches.list", {}))).toContain(saved.id);

  // Saving WITH an id updates in place. Asserting on the id as well as the
  // name is the point: a save that created a second row would leave the new
  // name findable and still be wrong.
  const updated = await rpc("savedSearches.save", {
    id: saved.id,
    name: `search-${RUN}-renamed`,
    queryJson: { field: "status", operator: "eq", value: "RESOLVED" },
  });
  expect(updated.id, "updating must not create a second row").toBe(saved.id);
  expect(updated.name).toBe(`search-${RUN}-renamed`);
  const afterUpdate = await rpc("savedSearches.list", {});
  expect(afterUpdate.filter((row: { id: string }) => row.id === saved.id)).toHaveLength(1);

  await rpc("savedSearches.delete", { id: saved.id });
  expect(ids(await rpc("savedSearches.list", {}))).not.toContain(saved.id);
});

test("the unread count rises on someone else's change and falls when read", async ({
  page,
  baseURL,
  context,
  browser,
}) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", { key: productKey("NOTI"),
    name: `Notify ${RUN}`, description: "n" });
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
    summary: `Watch this ${RUN}`,
    description: "d",
  });
  const me = await rpc("users.me", {});
  await rpc("cc.add", { bugId: bug.id, userId: me.id });

  // Someone ELSE has to make the change. Your own edits are deliberately not
  // notified, so a single-identity version of this test would assert that the
  // count does not move and pass for the wrong reason.
  const bobContext = await browser.newContext();
  await signIn(bobContext, { runtimePort: RUNTIME_PORT, baseURL: baseURL!, user: otherUser });
  const bob = await bobContext.newPage();

  // `.count`, not the object. `notifications.unreadCount` returns
  // `{ count, cached }`, and comparing the objects compares `[object Object]`
  // to itself -- an assertion that holds no matter what the count does.
  const countNow = async () => (await rpc("notifications.unreadCount", {})).count as number;

  const before = await countNow();
  await bob.request.post(`${baseURL}/__zeroship/v1/comments.add`, {
    data: { json: { bugId: bug.id, body: `bob was here ${RUN}` } },
  });
  expect(await countNow(), "a CC'd user is notified of someone else's comment").toBe(before + 1);

  const unread = await rpc("notifications.list", { unreadOnly: true });
  const mine = unread.filter((row: { bugId: string }) => row.bugId === bug.id);
  expect(mine, "the new notification is about this bug").toHaveLength(1);

  await rpc("notifications.markRead", { id: mine[0].id });
  expect(await countNow(), "marking it read must decrement, not just hide it").toBe(before);

  await bobContext.close();
});
