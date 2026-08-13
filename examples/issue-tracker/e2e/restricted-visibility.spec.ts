import { expect, test } from "@playwright/test";

import { otherUser, signIn } from "./session";
import { chooseOption } from "./select";
import { productKey } from "./keys";
import { openMorePanels } from "./more";

/**
 * The access-control model, driven in two real browsers.
 *
 * WHY THIS EXISTS SEPARATELY. bug-lifecycle.spec.ts says so in its own header:
 * "It runs one identity, so nothing here shows that another user is denied."
 * Its security spec restricts a bug through the UI and then asserts on the
 * confirmation text -- but that string is a client-side literal set after the
 * call resolves (SecurityPanel.tsx), so it proves `restrictBug` did not throw
 * and nothing more. A server that accepted the call and wrote nothing would
 * pass it. Excluding somebody is the entire point of the feature, and no
 * browser spec showed it happening.
 *
 * THE SHAPE. Each assertion below is one half of a pair that differs in exactly
 * one variable -- the restriction. Bob is the same person, in the same browser,
 * on the same URL, before and after. Without the "before" half a failure to
 * render for any other reason (bad id, unprovisioned user, dead route) would
 * read as a working access check.
 *
 * WHAT IT STILL DOES NOT COVER. Both identities are planted dev-session
 * cookies, so the login form is untested here too, and the dev tier means the
 * gateway's JWT path is not involved. Group MEMBERSHIP is not exercised: Bob is
 * in no group, so this shows non-members are excluded, not that members are let
 * back in. scripts/smoke.sh covers that arm over RPC.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

test("a bug restricted to a group stops being visible to a non-member", async ({
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

  // Alice writes BEFORE Bob's context exists, and that ordering is load-bearing
  // rather than incidental: the app makes the first account to reach a handler
  // the administrator, and an administrator bypasses group restrictions
  // outright (`canViewBug`). If Bob provisioned first he would see the bug
  // after the restriction and this spec would fail while the app was right.
  const product = await rpc("products.create", { key: productKey("VIS"),
    name: `Vis ${RUN}`, description: "visibility" });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const summary = `Embargoed ${RUN}`;
  const bug = await rpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary,
    description: "sensitive",
  });

  const bobContext = await browser.newContext();
  await signIn(bobContext, { runtimePort: RUNTIME_PORT, baseURL: baseURL!, user: otherUser });
  const bob = await bobContext.newPage();

  const openDetail = async () => {
    await bob.goto(`/#/bugs/${bug.id}`);
    // A full reload, not just a hash change. Arriving from the list is a
    // same-document navigation, so without this the second visit could render
    // whatever the router already had rather than refetching.
    await bob.reload();
  };
  const search = async () => {
    await bob.goto("/#/bugs");
    await bob.reload();
    await bob.getByPlaceholder("Search, or type").fill(RUN);
    await bob.getByRole("button", { name: "Apply" }).click();
  };

  // ---- Before: the control half. -----------------------------------------
  await openDetail();
  await expect(bob.locator("h1"), "Bob should see an unrestricted bug").toContainText(summary);
  await search();
  await expect(bob.getByText(summary), "and should find it in the list").toBeVisible();

  // ---- The one variable: Alice restricts it, through the UI. --------------
  await page.goto("/#/products");
  const groups = page.locator("section.groups-admin");
  await expect(groups).toBeVisible();
  await groups.getByLabel("New group").fill(`vis-${RUN}`);
  await groups.getByRole("button", { name: "Create" }).click();
  await expect(groups.locator("ul.group-list").getByText(`vis-${RUN}`)).toBeVisible();

  await page.goto(`/#/bugs/${bug.id}`);
  await openMorePanels(page);
  const security = page.locator("section.security-panel");
  await expect(security).toBeVisible();
  await chooseOption(page, security, "Group", `vis-${RUN}`);
  await security.getByRole("button", { name: "Restrict", exact: true }).click();
  await expect(security.getByText(/only members of that group/i)).toBeVisible();

  // ---- After: the same person, browser and URL. --------------------------
  await openDetail();
  // The absence of the summary comes FIRST, because it is the claim: whatever
  // the page decides to render, the restricted text must not be in it.
  //
  // By COUNT, not `not.toContainText` on the heading. The refused page renders
  // no h1 at all, so a negated text matcher fails with "element(s) not found"
  // -- it reports a problem precisely when the app is most correct. A count of
  // zero survives the summary being absent rather than merely different, and it
  // covers the whole page rather than one node.
  await expect(bob.getByText(summary), "Bob should be refused after the restriction").toHaveCount(
    0,
  );
  // The refusal must also name the real reason. This assertion was written
  // expecting "not found" and caught the app saying "You do not have access to
  // this product" -- false, and misleading in the exact case it fires: Bob
  // keeps full access to the product, as the search below still proves.
  // Asserting on the STATUS alone would have passed on that message.
  //
  // Matched by its text rather than by `getByRole("alert")`. The bug page
  // mounts a panel per section and each renders its own alert on failure, so
  // the role selector is ambiguous exactly when something has gone wrong -- it
  // reported a strict-mode violation about Dependencies and Duplicates instead
  // of the access failure under test.
  await expect(
    bob.getByText(/restricted to a group you are not in/i),
    "and should be told why",
  ).toBeVisible();

  await search();
  await expect(bob.getByText(summary), "and it should be gone from the list").toHaveCount(0);

  // Alice, in the same state, still sees it. Without this the "after" half is
  // also satisfied by a tracker that broke for everyone.
  await page.goto(`/#/bugs/${bug.id}`);
  await page.reload();
  await expect(page.locator("h1"), "the restriction must not hide it from a member").toContainText(
    summary,
  );

  // Lift the restriction so the group stops being load-bearing. The global
  // teardown deletes fixture groups through the guarded procedure, which
  // rightly refuses while a restriction is still attached -- so a spec that
  // restricts and walks away leaves a group nothing can ever clean up.
  const created = ((await rpc("groups.list", {})) as Array<{ id: string; name: string }>).find(
    (g) => g.name === `vis-${RUN}`,
  );
  if (created) await rpc("bugs.unrestrict", { bugId: bug.id, groupId: created.id });

  await bobContext.close();
});
