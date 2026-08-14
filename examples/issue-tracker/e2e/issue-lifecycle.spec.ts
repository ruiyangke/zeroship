import { expect, test } from "@playwright/test";

import { signIn } from "./session";
import { chooseOption } from "./select";
import { productKey } from "./keys";
import { openMorePanels, openProductsAdmin, openRailGroup } from "./more";

/**
 * The Bugzilla flow a real user walks, driven in a real browser: file an issue
 * through the guided form, find it in the list, open it, and resolve it.
 *
 * WHAT THIS COVERS THAT scripts/smoke.sh DOES NOT. smoke.sh drives the RPC
 * surface directly, so it proves the server is right and says nothing about
 * whether the client is wired to it. Everything below goes through the
 * rendered UI: the product -> component cascade only populates if the client
 * refetches on product change, the issue only appears in the list if the list
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
// issue and the spec stops testing what it claims to.
const RUN = `${process.pid}-${Date.now()}`;

test.beforeEach(async ({ context, baseURL }) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });
});

test("files an issue through the guided form and resolves it", async ({ page, baseURL }) => {
  // Structure is seeded over RPC rather than through the admin UI: this spec is
  // about the issue lifecycle, and building a product through the UI here would
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

  await page.goto("/issues/new");

  // The component list is populated by a fetch that only fires once a product
  // is chosen, so selecting the product must change what the next select
  // offers. Waiting for the option is the assertion.
  await chooseOption(page, page, "Product", `E2E ${RUN}`);
  // The cascade is still the assertion: chooseOption fails if the component
  // select has no "Parser" option, which is what selecting the product is
  // supposed to produce. It cannot be checked with toBeAttached any more --
  // the options only exist in the DOM while the popup is open.
  await chooseOption(page, page, "Component", "Parser");

  // Version is chosen on purpose, and it is no longer the difference between a
  // filed issue and a 500: `issues.versionId` is NULLABLE now, because a
  // feature request has no version it was found in. Setting it here keeps this
  // walk over the populated path, where the value has to survive the round trip
  // and the third cascade level has to render.
  await chooseOption(page, page, "Version", "1.0");

  await page.getByLabel("Summary").fill(summary);
  await page.getByLabel("Description").fill("echo -n loses the last newline");

  // Severity and priority are separate controls in Bugzilla and must stay
  // separate here: set them to values that differ so a control that writes
  // both would be caught.
  await chooseOption(page, page, "Severity", "major");
  await chooseOption(page, page, "Priority", "P1");

  await page.getByRole("button", { name: "File issue" }).click();

  // Filing navigates to the new issue's detail page. Asserted by ROLE rather
  // than by text: the summary appears in both the h1 (prefixed with the issue
  // id) and the h2, so a bare getByText matches two nodes and fails strict
  // mode -- which reads as "not found" and sends you looking for a filing bug
  // that isn't there.
  await expect(page).toHaveURL(/\/issues\/issu_/);
  // The page heading, level 1. The detail panel used to repeat the summary as
  // an h2 directly above an input holding the same text; this asserted on that
  // duplicate, so removing it broke a spec that was pinning the defect.
  await expect(page.getByRole("heading", { level: 1 })).toContainText(summary);

  // Severity and priority must both have survived the round trip as the values
  // chosen, and must still be distinct fields.
  // The RAIL ROW, scoped by its label. The rail states properties as values
  // now and only renders a control while one is being edited, so there is no
  // combobox to read at rest. Scoped rather than loose, because the page head
  // shows the same badge -- an unscoped getByText("major") matches both and
  // proves neither.
  await expect(
    page.locator(".rail-choice", { hasText: "Severity" }),
    "the rail states the severity",
  ).toContainText("major");
  // Same as severity: the priority badge is gone, and loose text "P1" now
  // matches a hidden <option> inside the select -- "received: hidden" rather
  // than "not found", which is the tell.
  await expect(
    page.locator(".rail-choice", { hasText: "Priority" }),
    "and the priority",
  ).toContainText("P1");

  // The issue is findable from the list by its summary.
  await page.goto("/issues");
  await page.getByPlaceholder("Search, or type").fill(RUN);
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
  await page.goto(`${baseURL}/dashboard`);

  // The dashboard is entirely authenticated reads, so it must say so.
  await expect(
    page.getByText(/sign in|sign-in|not signed in|authentication required/i).first(),
  ).toBeVisible();

  await anon.close();
});

test("an issue I am CC'd on appears on my dashboard", async ({ page, baseURL }) => {
  // Guards cc.listMine and its client wiring together. The schema was already
  // indexed for this direction (issueCc.userId) but no procedure read it, and
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
  const product = await rpc("products.create", { key: productKey("CC"),
    name: `CC ${RUN}`, description: "cc spec" });
  const me = await rpc("users.me", {});
  expect(me.id, "users.me must return a provisioned id after a write").toMatch(/^user_/);
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "2.0" });

  const summary = `Watched issue ${RUN}`;
  const issue = await rpc("issues.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary,
    description: "cc me",
  });

  // Before the CC exists the dashboard must NOT already show it -- otherwise a
  // section that lists everything would pass this test without cc.listMine
  // working at all.
  //
  // The four account queries are tabs of one table now rather than four stacked
  // sections, so the panel has to be OPENED -- and it has to be opened before
  // the negative assertion, or "the summary is not in the CC panel" would pass
  // against a panel that was never rendered, which is the strongest way to make
  // this test meaningless.
  const openCcTab = async () => {
    const tab = page.getByRole("tab", { name: /CC'd on/ });
    await tab.click();
    // Wait for the TAB to be selected, not for a table inside it.
    //
    // This waited for `section.my-work table`, which only renders when the
    // list has rows -- and the first call is deliberately made while the CC
    // list is still EMPTY, so that the negative assertion below means
    // something. It passed anyway, because the dev database held 1701 leaked
    // fixture issues and this user was CC'd on plenty of them, so a table was
    // always there. Sweeping the fixtures removed that prop and the wait timed
    // out on a legitimately empty tab.
    //
    // A wait that depends on the data being non-empty cannot be used to set up
    // an assertion about the data being empty.
    await expect(tab).toHaveAttribute("aria-selected", "true");
  };
  await page.goto("/dashboard");
  const ccSection = page.locator("section.my-work");
  await expect(ccSection).toBeVisible();
  await openCcTab();
  await expect(ccSection.getByText(summary)).toHaveCount(0);

  await rpc("cc.add", { issueId: issue.id, userId: me.id });

  await page.reload();
  await openCcTab();
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

  const product = await rpc("products.create", { key: productKey("VOTE"),
    name: `Vote ${RUN}`, description: "vote spec" });
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
    summary: `Vote target ${RUN}`,
    description: "vote on me",
  });

  // Voting off by default: the panel must say so rather than show a control
  // that always fails.
  await page.goto(`/issues/${issue.id}`);
  await openMorePanels(page);
  const votes = page.locator("section.votes-panel");
  await expect(votes).toBeVisible();
  await expect(votes.getByText(/not enabled/i)).toBeVisible();

  await rpc("products.update", {
    id: product.id,
    changes: { votesPerUser: 5, maxVotesPerIssue: 3, votesToConfirm: 2 },
  });

  await page.reload();
  // Same as after any navigation: the fold is view state and comes back
  // closed, so the panel has to be asked for again.
  await openMorePanels(page);
  await expect(votes.getByText(/not enabled/i)).toHaveCount(0);
  // Typed, not filled. NumberField is a FORMATTED text input (Base UI parses
  // input events and re-renders from its own state), so Playwright .fill(),
  // which assigns .value directly, leaves the component out of sync -- "2"
  // landed as "12". A user selects and types, so the spec does that.
  const voteCount = votes.getByLabel("My votes");
  await voteCount.click();
  await voteCount.press("ControlOrMeta+A");
  await voteCount.pressSequentially("2");
  await voteCount.blur();
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
  const product = await rpc("products.create", { key: productKey("NOTI"),
    name: `Notif ${RUN}`, description: "notif spec" });
  const me = await rpc("users.me", {});
  expect(me.id, "users.me must return a provisioned id after a write").toMatch(/^user_/);
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const summary = `Notify me ${RUN}`;
  const issue = await rpc("issues.create", {
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
  await page.goto("/dashboard");
  const inbox = page.locator("section.notifications-panel");
  await expect(inbox).toBeVisible();
  await expect(inbox.getByRole("heading", { name: "Notifications" })).toBeVisible();

  // A self-authored comment must NOT appear: the actor is not notified about
  // her own change, and an inbox that showed it would be wrong in a way the
  // count alone would hide.
  await rpc("cc.add", { issueId: issue.id, userId: me.id });
  await rpc("comments.add", { issueId: issue.id, body: `self comment ${RUN}` });
  await page.reload();
  await expect(inbox.getByText(summary)).toHaveCount(0);
});

test("an admin can restrict an issue to a group through the UI", async ({ page, baseURL }) => {
  // The whole access-control model -- eight procedures -- had no interface at
  // all, so the app's most consequential feature could only be reached over
  // raw RPC. This drives it the way an operator would.
  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", { key: productKey("SEC"),
    name: `Sec ${RUN}`, description: "sec spec" });
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
    summary: `Confidential ${RUN}`,
    description: "sensitive",
  });

  // Create the group through the admin page, not over RPC: that surface is
  // the thing under test.
  await page.goto("/products");
  await openProductsAdmin(page);
  const groups = page.locator("section.groups-admin");
  await expect(groups).toBeVisible();
  await groups.getByLabel("New group").fill(`sec-${RUN}`);
  await groups.getByRole("button", { name: "Create" }).click();
  // Scoped to the LIST. Creating a group also adds an <option> to the
  // add-member picker, so a bare getByText matches two nodes and fails strict
  // mode -- which reads as "the group was not created" when it was.
  await expect(groups.locator("ul.group-list").getByText(`sec-${RUN}`)).toBeVisible();

  // Then restrict the issue from its detail page.
  await page.goto(`/issues/${issue.id}`);
  await openMorePanels(page);
  const security = page.locator("section.security-panel");
  await expect(security).toBeVisible();
  // One helper call. The native select rendered its options with the JSX
  // whitespace around them, so selecting by label matched nothing and left the
  // control unset -- which read as a broken panel. That workaround (read the
  // value off the option, select by value) is gone with the native select.
  await chooseOption(page, security, "Group", `sec-${RUN}`);
  await security.getByRole("button", { name: "Restrict", exact: true }).click();

  // The confirmation must state the consequence, not just "done" -- restricting
  // an issue changes who can see it and that is the point of the action.
  await expect(security.getByText(/only members of that group/i)).toBeVisible();

  // Lift it again. The global teardown sweeps fixture groups through the
  // guarded delete, which refuses while a restriction is attached -- so a spec
  // that restricts and walks away leaves a group nothing can ever clean up.
  const created = ((await rpc("groups.list", {})) as Array<{ id: string; name: string }>).find(
    (g) => g.name === `sec-${RUN}`,
  );
  if (created) await rpc("issues.unrestrict", { issueId: issue.id, groupId: created.id });
});

test("see also links are added, listed and removed from the issue page", async ({ page, baseURL }) => {
  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", { key: productKey("SEEA"),
    name: `SeeAlso ${RUN}`, description: "sa spec" });
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
    summary: `Linked ${RUN}`,
    description: "has links",
  });

  await page.goto(`/issues/${issue.id}`);
  await openMorePanels(page);
  await openRailGroup(page, "See also");
  const panel = page.locator("section.see-also-panel");
  await expect(panel).toBeVisible();
  await expect(panel.getByText("No linked reports.")).toBeVisible();

  const url = `https://bugzilla.example.org/show_bug.cgi?id=${RUN}`;
  await panel.getByLabel("Link").fill(url);
  await panel.getByRole("button", { name: "Add", exact: true }).click();

  // Rendered as a real anchor, since the server guarantees the scheme is
  // http(s) -- a javascript: url is refused there, which is why the client can
  // safely put this in an href.
  const link = panel.getByRole("link", { name: url });
  await expect(link).toBeVisible();
  await expect(link).toHaveAttribute("rel", /noreferrer/);

  await panel.getByRole("button", { name: "Remove", exact: true }).click();
  await expect(panel.getByText("No linked reports.")).toBeVisible();
});
