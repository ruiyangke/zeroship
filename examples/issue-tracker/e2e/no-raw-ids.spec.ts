import { expect, test } from "@playwright/test";

import { signIn } from "./session";
import { productKey } from "./keys";
import { openMorePanels, openRailGroup } from "./more";

/**
 * No page shows a raw typed_id where a name belongs.
 *
 * This defect keeps coming back in different clothes, which is why it is a
 * test rather than a fixed bug. The id column showed a truncated UUID; the
 * dashboard and both search tables showed `prod_...` and `user_...` because a
 * lookup prop was optional and three call sites omitted it; the issue page
 * printed `user_...` for the assignee, reporter and every comment author; and
 * the moment the dropdowns became comboboxes, every id-valued Select rendered
 * its value instead of its label.
 *
 * Each of those was found by looking at a screenshot. This asserts it.
 *
 * VISIBLE TEXT ONLY. A typed_id in an `href` or a `title` is correct -- the
 * row links by id and the anchor carries the full one deliberately -- so this
 * reads rendered text and ignores attributes.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

// Every entity prefix the app mints. Deliberately broad: a new table gets a
// new prefix, and the point is to catch the ones nobody thought about.
//
// THESE ARE DERIVED, NOT CHOSEN. No table in migrations/ declares an id
// prefix, so the platform computes one per collection: strip a trailing "s",
// take the first four alphanumerics, lowercase
// (`derive_prefix_from_collection_name`, crates/plugin-db/src/crud/
// system_fields_pass.rs). `issues` therefore mints `issu_`, `groups` mints
// `grou_`, `comments` mints `comm_`. A guessed abbreviation matches nothing,
// and an alternative that matches nothing is an alternative that cannot fail:
// the list carried `bug`, `grp`, `kw`, `att`, `cmt` and `note`, of which only
// `bug` was ever real, and the `bugs` -> `issues` rename retired that one too.
// The pin below keeps the whole list honest against at least one live id.
const RAW_ID =
  /\b(prod|comp|vers|mile|user|pws|grou|issu|comm|atta|keyw|vote|flag|acti|save|watc|noti)_[A-Za-z0-9]{12,}/;

const PAGES: [string, string][] = [
  ["issue list", "/issues"],
  ["new issue", "/issues/new"],
  ["dashboard", "/dashboard"],
  ["products admin", "/products"],
  ["reports", "/reports"],
];

test("no page renders a raw typed id as visible text", async ({ page, baseURL, context }) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  // An issue with everything hung off it, so the detail page has something in
  // each panel. An empty page cannot show an id it never renders.
  const product = await rpc("products.create", {
    name: `Ids ${RUN}`,
    key: productKey("IDS"),
    description: "ids",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  await rpc("milestones.create", { productId: product.id, name: "M1" });
  const issue = await rpc("issues.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Every panel ${RUN}`,
    description: "d",
  });
  const me = await rpc("users.me", {});
  // This whole test is an absence assertion, so it is worth nothing if RAW_ID
  // has drifted off the shape the app actually mints. Two live ids from two
  // different collections, checked before the sweep runs.
  expect(issue.id, "RAW_ID no longer matches a real issue id").toMatch(RAW_ID);
  expect(me.id, "RAW_ID no longer matches a real user id").toMatch(RAW_ID);
  await rpc("issues.reassign", { id: issue.id, assigneeId: me.id });
  await rpc("cc.add", { issueId: issue.id, userId: me.id });
  await rpc("comments.add", { issueId: issue.id, body: "a comment" });
  await rpc("seeAlso.add", { issueId: issue.id, url: "https://example.com/1" });
  // A DEPENDENCY, which this fixture claimed to have and did not.
  //
  // "Everything hung off it" was the stated premise, but nothing here ever
  // called deps.add, so no dependsOn row was written and the one activity
  // whose VALUE is another issue's id never rendered. The history duly printed
  // "set Depends On to issu_0346W0Ole6amXDN9RKvzW6" for months underneath a
  // green test asserting that could not happen.
  const blocker = await rpc("issues.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Blocker ${RUN}`,
    description: "d",
  });
  await rpc("deps.add", { issueId: issue.id, dependsOnId: blocker.id });
  const keyword = await rpc("keywords.create", { name: `kw-${RUN}` });
  await rpc("keywords.attach", { issueId: issue.id, keywordId: keyword.id });
  // And a DUPLICATE, the other half of the same omission. The duplicates
  // panel states "this issue is a duplicate of X" straight from the column, and
  // printed the raw id as the link's text -- invisible to this spec for as
  // long as its issue was a duplicate of nothing.
  await rpc("issues.markDuplicate", { id: issue.id, duplicateOfId: blocker.id });

  const offenders: string[] = [];
  const check = async (label: string) => {
    // innerText, not textContent: it returns what is RENDERED, so a hidden
    // <option> or an aria-only node cannot fail this, and a visible id cannot
    // hide from it.
    const text = await page.evaluate(() => document.body.innerText);
    const hit = text.match(RAW_ID);
    if (hit) offenders.push(`${label}: ${hit[0]}`);
  };

  for (const [label, path] of PAGES) {
    await page.goto(path);
    await page.waitForTimeout(1200);
    await check(label);
  }

  await page.goto(`/issues/${issue.id}`);
  await page.waitForTimeout(1600);
  await check("issue detail");

  // OPEN what is collapsed, or this audits the page's resting state only.
  //
  // The rail's groups render nothing until asked, so when labels, CC and the
  // relations moved there, everything inside them left this spec's reach in
  // the same change. It went green against a duplicates panel printing a raw
  // id -- the panel was simply not in the DOM. A sweep of rendered text can
  // only see what is rendered, so it has to do the opening a person does.
  for (const group of ["Labels", "CC", "Dependencies", "Duplicates", "See also"]) {
    await openRailGroup(page, group);
    await check(`rail: ${group}`);
  }
  await openMorePanels(page);
  await page.waitForTimeout(600);
  await check("folded panels");
  // The history tab renders activity rows whose values are ids for several
  // fields, which is the most likely place for one to surface.
  await page.getByRole("tab", { name: /^History/ }).click();
  await page.waitForTimeout(800);
  await check("issue history");

  expect(offenders, `raw ids rendered as text:\n${offenders.join("\n")}`).toEqual([]);
});
