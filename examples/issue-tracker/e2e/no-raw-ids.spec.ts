import { expect, test } from "@playwright/test";

import { signIn } from "./session";
import { productKey } from "./keys";

/**
 * No page shows a raw typed_id where a name belongs.
 *
 * This defect keeps coming back in different clothes, which is why it is a
 * test rather than a fixed bug. The id column showed a truncated UUID; the
 * dashboard and both search tables showed `prod_...` and `user_...` because a
 * lookup prop was optional and three call sites omitted it; the bug page
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
const RAW_ID = /\b(prod|comp|vers|mile|user|pws|grp|bug|kw|flag|att|cmt|note)_[A-Za-z0-9]{12,}/;

const PAGES: [string, string][] = [
  ["bug list", "/#/bugs"],
  ["new bug", "/#/bugs/new"],
  ["dashboard", "/#/dashboard"],
  ["products admin", "/#/products"],
  ["reports", "/#/reports"],
];

test("no page renders a raw typed id as visible text", async ({ page, baseURL, context }) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  // A bug with everything hung off it, so the detail page has something in
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
  const bug = await rpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Every panel ${RUN}`,
    description: "d",
  });
  const me = await rpc("users.me", {});
  await rpc("bugs.reassign", { id: bug.id, assigneeId: me.id });
  await rpc("cc.add", { bugId: bug.id, userId: me.id });
  await rpc("comments.add", { bugId: bug.id, body: "a comment" });
  await rpc("seeAlso.add", { bugId: bug.id, url: "https://example.com/1" });
  // A DEPENDENCY, which this fixture claimed to have and did not.
  //
  // "Everything hung off it" was the stated premise, but nothing here ever
  // called deps.add, so no dependsOn row was written and the one activity
  // whose VALUE is another bug's id never rendered. The history duly printed
  // "set Depends On to bug_0346W0Ole6amXDN9RKvzW6" for months underneath a
  // green test asserting that could not happen.
  const blocker = await rpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Blocker ${RUN}`,
    description: "d",
  });
  await rpc("deps.add", { bugId: bug.id, dependsOnId: blocker.id });
  const keyword = await rpc("keywords.create", { name: `kw-${RUN}` });
  await rpc("keywords.attach", { bugId: bug.id, keywordId: keyword.id });

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

  await page.goto(`/#/bugs/${bug.id}`);
  await page.waitForTimeout(1600);
  await check("bug detail");
  // The history tab renders activity rows whose values are ids for several
  // fields, which is the most likely place for one to surface.
  await page.getByRole("button", { name: /^History/ }).click();
  await page.waitForTimeout(800);
  await check("bug history");

  expect(offenders, `raw ids rendered as text:\n${offenders.join("\n")}`).toEqual([]);
});
