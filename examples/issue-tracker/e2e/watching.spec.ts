import { expect, test } from "@playwright/test";

import { otherUser, signIn } from "./session";
import { productKey } from "./keys";

/**
 * User watching, end to end: watch someone, hear about their bug, stop.
 *
 * The three procedures behind this had no UI, so the feature was unreachable
 * -- but unlike the flags surface it was not dead. `notifyBugChange` already
 * read the `watchers` table and fanned out to them alongside the assignee,
 * reporter, QA contact and CC list. Only the way to say who you watch was
 * missing.
 *
 * That distinction is why this spec asserts on the NOTIFICATION rather than
 * the list of people. A name appearing in a panel proves a row was written;
 * only an unread count moving proves the watch does what it claims.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

test("watching a user delivers their bug activity, and stopping ends it", async ({
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

  // Alice writes first, so she is the admin and Bob is an ordinary user.
  // She creates nothing Bob will file into: components.create stamps its
  // creator as the default assignee, so a bug filed into HER component is
  // assigned to her and she is notified as the assignee -- watch or no watch.
  // That made the "no notification after unwatching" half unfalsifiable.
  await rpc("products.create", {
    name: `Watch ${RUN}`,
    key: productKey("WCH"),
    description: "watching",
  });

  const bobContext = await browser.newContext();
  await signIn(bobContext, { runtimePort: RUNTIME_PORT, baseURL: baseURL!, user: otherUser });
  const bob = await bobContext.newPage();
  // Guarded like the other helper. Unchecked, a non-200 came back as
  // `undefined` and surfaced later as a TypeError on `.id`, or -- worse --
  // as the notification assertion failing for a reason that had nothing to do
  // with watching. A helper that swallows the status turns "the procedure is
  // gone" into "the feature is broken", which is a long way to walk back.
  const bobRpc = async (proc: string, json: unknown) => {
    const res = await bob.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} (as Bob) should succeed`).toBe(200);
    return (await res.json()).json;
  };
  // Bob has to exist as an app user before he can be watched, and only a
  // write provisions him.
  await bobRpc("users.updatePrefs", { timezone: "UTC" });
  // BOB owns the product, component and version, so the only route from his
  // bug to Alice is the watch.
  const product = await bobRpc("products.create", {
    name: `Watched ${RUN}`,
    key: productKey("WBB"),
    description: "bob owns this",
  });
  const component = await bobRpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await bobRpc("versions.create", { productId: product.id, name: "1.0" });

  const countNow = async () => (await rpc("notifications.unreadCount", {})).count as number;

  // Teardown over RPC, not the UI. The dev database persists between runs, so
  // a run that fails midway leaves a watch behind; clearing it by clicking
  // makes the SETUP part of what is under test, and a stale row then reads as
  // a broken Stop button.
  const bobUser = (await rpc("users.list", { text: otherUser.email }))[0];
  for (const row of await rpc("watchers.list", {})) {
    await rpc("watchers.remove", { watchedId: row.watchedId });
  }
  expect(bobUser, "Bob must exist as an app user before he can be watched").toBeTruthy();

  await page.goto("/#/dashboard");
  const panel = page.locator("section.watching-panel");
  await expect(panel).toBeVisible();
  const bobRow = panel.locator("ul.member-list li", { hasText: otherUser.name });
  await expect(bobRow, "the fixture starts with Bob unwatched").toHaveCount(0);

  // The WATCH is established over RPC. Driving the people picker here made
  // the setup part of what is under test, and a picker that returned a
  // different first result then read as a broken watch. The claim is about
  // notifications, so that is what the UI part asserts against.
  await rpc("watchers.add", { watchedId: bobUser.id });
  await page.reload();
  await expect(bobRow, "the panel shows who you watch").toHaveCount(1);

  // Bob files a bug Alice has no other connection to: not her product to
  // report, not assigned to her, not on the CC list. The only reason she
  // should hear about it is the watch.
  const before = await countNow();
  await bobRpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Bob filed this ${RUN}`,
    description: "watched activity",
  });
  expect(await countNow(), "a watcher is notified of the watched user's bug").toBeGreaterThan(
    before,
  );

  // And stopping actually stops it.
  // Stopping IS under test -- it is the button this panel exists to provide.
  await page.reload();
  await bobRow.getByRole("button", { name: "Stop watching" }).click();
  await expect(bobRow, "the watch is gone").toHaveCount(0);

  const afterStop = await countNow();
  await bobRpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Bob filed another ${RUN}`,
    description: "no longer watched",
  });
  expect(await countNow(), "no notification once the watch is removed").toBe(afterStop);

  await bobContext.close();
});
