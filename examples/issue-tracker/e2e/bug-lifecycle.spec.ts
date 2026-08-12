import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * The Bugzilla flow a real user walks, driven in a real browser: file a bug
 * through the guided form, find it in the list, open it, and resolve it.
 *
 * WHAT THIS COVERS THAT scripts/smoke.sh DOES NOT. smoke.sh drives the RPC
 * surface directly, so it proves the server is right and says nothing about
 * whether the client is wired to it. Everything below goes through the
 * rendered UI: the product -> component cascade only populates if the client
 * refetches on product change, the bug only appears in the list if the list
 * reads the same filter the form wrote, and the detail page only shows a
 * resolution if the client sent one.
 *
 * WHAT IT DOES NOT COVER. It signs in by planting the dev session cookie, so
 * the login form itself is never exercised. It runs one identity, so nothing
 * here shows that another user is denied. And it runs against the dev tier, so
 * the gateway's JWT validation and route dispatch are not involved.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

// A per-run marker so repeated runs against the same dev database cannot see
// each other's rows. Without it the list assertions pass on a previous run's
// bug and the spec stops testing what it claims to.
const RUN = `${process.pid}-${Date.now()}`;

test.beforeEach(async ({ context, baseURL }) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });
});

test("files a bug through the guided form and resolves it", async ({ page, baseURL }) => {
  // Structure is seeded over RPC rather than through the admin UI: this spec is
  // about the bug lifecycle, and building a product through the UI here would
  // mean a failure in the products page reports as a failure in this test.
  //
  // `page.request` and NOT the `request` fixture: the fixture is a separate
  // APIRequestContext that does not carry the browser context's cookies, so
  // every seed call went out unauthenticated and came back 401.
  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `E2E ${RUN}`,
    description: "browser lifecycle spec",
  });
  await rpc("components.create", { productId: product.id, name: "Parser", description: "parser" });
  await rpc("versions.create", { productId: product.id, name: "1.0" });

  const summary = `Parser drops trailing newline ${RUN}`;

  await page.goto("/#/bugs/new");

  // The component list is populated by a fetch that only fires once a product
  // is chosen, so selecting the product must change what the next select
  // offers. Waiting for the option is the assertion.
  await page.getByLabel("1. Product").selectOption({ label: `E2E ${RUN}` });
  const componentSelect = page.getByLabel("2. Component");
  await expect(componentSelect.getByRole("option", { name: "Parser" })).toBeAttached();
  await componentSelect.selectOption({ label: "Parser" });

  // Version is required because bugs.versionId is NOT NULL. Selecting it here
  // is not incidental setup: leaving the form's default in place is exactly
  // what produced an unfileable bug and a 500.
  await page.getByLabel("Version").selectOption({ label: "1.0" });

  await page.getByLabel("Summary").fill(summary);
  await page.getByLabel("Description").fill("echo -n loses the last newline");

  // Severity and priority are separate controls in Bugzilla and must stay
  // separate here: set them to values that differ so a control that writes
  // both would be caught.
  await page.getByLabel("Severity").selectOption("major");
  await page.getByLabel("Priority").selectOption("P1");

  await page.getByRole("button", { name: "File bug" }).click();

  // Filing navigates to the new bug's detail page. Asserted by ROLE rather
  // than by text: the summary appears in both the h1 (prefixed with the bug
  // id) and the h2, so a bare getByText matches two nodes and fails strict
  // mode -- which reads as "not found" and sends you looking for a filing bug
  // that isn't there.
  await expect(page).toHaveURL(/#\/bugs\/bug_/);
  await expect(page.getByRole("heading", { level: 2, name: summary })).toBeVisible();

  // Severity and priority must both have survived the round trip as the values
  // chosen, and must still be distinct fields.
  await expect(page.getByText("major", { exact: true }).first()).toBeVisible();
  await expect(page.getByText("P1", { exact: true }).first()).toBeVisible();

  // The bug is findable from the list by its summary.
  await page.goto("/#/bugs");
  await page.getByPlaceholder("Search summary, whiteboard, URL...").fill(RUN);
  await page.keyboard.press("Enter");
  await expect(page.getByText(summary)).toBeVisible();
});

test("an anonymous visitor is told to sign in rather than shown an empty list", async ({
  browser,
  baseURL,
}) => {
  // A fresh context with NO session cookie. This is the assertion the UI brief
  // called out: a 401 from a fail-closed procedure must not render as "no
  // results", because that reads as "there is nothing here" when the truth is
  // "you cannot see it".
  const anon = await browser.newContext();
  const page = await anon.newPage();
  await page.goto(`${baseURL}/#/dashboard`);

  // The dashboard is entirely authenticated reads, so it must say so.
  await expect(
    page.getByText(/sign in|sign-in|not signed in|authentication required/i).first(),
  ).toBeVisible();

  await anon.close();
});

test("a bug I am CC'd on appears on my dashboard", async ({ page, baseURL }) => {
  // Guards cc.listMine and its client wiring together. The schema was already
  // indexed for this direction (bugCc.userId) but no procedure read it, and
  // the dashboard rendered a hardcoded "(0)" with a note claiming the lookup
  // was impossible. Both halves have to work for this to pass.
  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  // The product is created FIRST, and `me` read after it, because `users.me`
  // does NOT provision: it returns id null until the identity has written
  // something. Reading it first made this spec depend on an EARLIER spec
  // having done a write -- it passed in a full run and failed under --grep or
  // sharding, on a fresh database, with a cc.add 400 that says nothing about
  // provisioning.
  const product = await rpc("products.create", { name: `CC ${RUN}`, description: "cc spec" });
  const me = await rpc("users.me", {});
  expect(me.id, "users.me must return a provisioned id after a write").toMatch(/^user_/);
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "2.0" });

  const summary = `Watched bug ${RUN}`;
  const bug = await rpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary,
    description: "cc me",
  });

  // Before the CC exists the dashboard must NOT already show it -- otherwise a
  // section that lists everything would pass this test without cc.listMine
  // working at all.
  await page.goto("/#/dashboard");
  const ccSection = page.locator("section.dashboard-section", { hasText: "Bugs I'm CC'd on" });
  await expect(ccSection).toBeVisible();
  await expect(ccSection.getByText(summary)).toHaveCount(0);

  await rpc("cc.add", { bugId: bug.id, userId: me.id });

  await page.reload();
  await expect(ccSection.getByText(summary)).toBeVisible();
});

test("voting is offered only when the product enables it", async ({ page, baseURL }) => {
  // The votes panel reads two product fields that were uneditable until
  // products.update learned about them, so this also pins that the limits can
  // actually be turned on through the API the UI depends on.
  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", { name: `Vote ${RUN}`, description: "vote spec" });
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
    summary: `Vote target ${RUN}`,
    description: "vote on me",
  });

  // Voting off by default: the panel must say so rather than show a control
  // that always fails.
  await page.goto(`/#/bugs/${bug.id}`);
  const votes = page.locator("section.votes-panel");
  await expect(votes).toBeVisible();
  await expect(votes.getByText(/not enabled/i)).toBeVisible();

  await rpc("products.update", {
    id: product.id,
    changes: { votesPerUser: 5, maxVotesPerBug: 3, votesToConfirm: 2 },
  });

  await page.reload();
  await expect(votes.getByText(/not enabled/i)).toHaveCount(0);
  await votes.getByLabel("My votes").fill("2");
  await votes.getByRole("button", { name: "Vote" }).click();

  // voteCount is the SUM of quantities, so two votes read as 2, not 1.
  await expect(votes.getByText("2 votes")).toBeVisible();
});

test("the notification inbox renders, and omits my own changes", async ({ page, baseURL }) => {
  // The nav has carried an unread badge since the UI was written, linking to a
  // dashboard with nowhere to read the notifications it counted. This drives
  // the whole chain: fanout writes a row, the inbox renders it, marking it
  // read removes the action.
  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  // Product first, `me` after: users.me does not provision (see the CC spec).
  const product = await rpc("products.create", { name: `Notif ${RUN}`, description: "notif spec" });
  const me = await rpc("users.me", {});
  expect(me.id, "users.me must return a provisioned id after a write").toMatch(/^user_/);
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const summary = `Notify me ${RUN}`;
  const bug = await rpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary,
    description: "watch this",
  });

  // SCOPE, stated because the name of this test used to overclaim it: the
  // actor is never notified about her own change, and this spec runs ONE
  // identity, so it cannot show a notification arriving. That direction is
  // covered by scripts/smoke.sh, which runs two identities and asserts the
  // unread count rises for the CC'd user. What this spec pins is the inbox
  // rendering at all, and the negative that a self-authored comment does not
  // appear in it.
  await page.goto("/#/dashboard");
  const inbox = page.locator("section.notifications-panel");
  await expect(inbox).toBeVisible();
  await expect(inbox.getByRole("heading", { name: "Notifications" })).toBeVisible();

  // A self-authored comment must NOT appear: the actor is not notified about
  // her own change, and an inbox that showed it would be wrong in a way the
  // count alone would hide.
  await rpc("cc.add", { bugId: bug.id, userId: me.id });
  await rpc("comments.add", { bugId: bug.id, body: `self comment ${RUN}` });
  await page.reload();
  await expect(inbox.getByText(summary)).toHaveCount(0);
});
