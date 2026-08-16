import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * The rail says "empty" one way, and never says it about a value it is still
 * fetching.
 *
 * A freshly filed issue has almost nothing set, so the right-hand column is
 * mostly absence -- and it had SEVEN spellings for it. `--` on QA contact and
 * the free-text fields, `None` on version and milestone, `none` on keywords,
 * duplicates and see-also, `nobody` on CC, `not set` on flags, `unassigned` on
 * assignee. Reading down one column meant learning seven words for the same
 * fact.
 *
 * Worse, `--` also meant "still loading" in CC and Dependencies, so the same
 * dim glyph in the same column stood for two unrelated things and the only way
 * to tell them apart was to wait and see whether it changed. Loading is a
 * Skeleton now, which is `aria-hidden` and cannot be mistaken for content.
 *
 * WHAT THIS DOES NOT CATCH, measured by mutation rather than guessed. Putting
 * `None` back on the milestone fallback fails this spec; putting it back on
 * the ASSIGNEE fallback does not, and that is not a gap in the assertions --
 * `issues.create` assigns the reporter, so no fixture here has an empty
 * assignee and the branch never renders. Reaching it needs an issue whose
 * assignee was explicitly cleared.
 *
 * Nor does it reach a spelling that only appears inside an opened panel, on a
 * issue with data, or on another page. `EmptyState` prose ("No keywords on this
 * issue.") is deliberately still prose, and the History tab still says "unset"
 * inside a sentence about a past value. The claim is about the rail's
 * at-a-glance column, not about every word for empty in the app.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("the rail spells absence one way, and never spells it while loading", async ({
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
    name: `Absent ${RUN}`,
    key: productKey("ABS"),
    description: "absence",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  // Nothing optional is set: no assignee, no QA contact, no milestone, no CC,
  // no keywords, no dependencies, no flags. The rail is almost all absence.
  const issue = await rpc("issues.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Nothing set ${RUN}`,
    description: "d",
  });

  await page.goto(`/issues/${issue.id}`);
  const rail = page.locator(".issue-detail-side");
  await expect(rail).toBeVisible();
  // The rail's async groups (CC, dependencies, duplicates, see-also) each
  // resolve on their own request. Wait for the placeholders to clear, or the
  // absence assertions below would race the very Skeletons this spec is about.
  await expect(rail.locator("[data-slot~=\"skeleton\"]")).toHaveCount(0, { timeout: 10_000 });

  const text = (await rail.innerText()).toLowerCase();

  // One token for empty. Each of these was a real spelling in this column.
  for (const word of ["none", "nobody", "not set", "unassigned"]) {
    expect(
      text,
      `the rail still spells absence "${word}"; it is "--" everywhere now`,
    ).not.toContain(word);
  }

  // And absence is actually SHOWN -- without this the loop above passes on a
  // rail that failed to render at all, which is the failure mode a
  // "does not contain" assertion cannot see on its own.
  expect(text, "the rail prints the absent token for the fields nothing set").toContain("--");

  // The dim class is what makes an absent value read as absent rather than as
  // a value that happens to be punctuation.
  const absentCount = await rail.locator("span.dim", { hasText: /^--$/ }).count();
  expect(absentCount, "absent values are dimmed, not printed at value weight").toBeGreaterThan(0);
});

test("a rail group shows a skeleton before its answer, not an absence", async ({
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
    name: `Slow ${RUN}`,
    key: productKey("SLW"),
    description: "slow",
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
    summary: `Slow rail ${RUN}`,
    description: "d",
  });

  // Hold the CC answer open. Without a delay the request resolves faster than
  // the assertion can run, and the spec would pass whether or not a skeleton
  // is ever rendered -- it would only prove the final state, which the test
  // above already covers.
  // A REGEX, not the glob `**/__zeroship/v1/cc.list`. Queries travel as GET
  // with the arguments in `?input=<base64>`, and Playwright matches a glob
  // against the whole URL including that query string -- so the glob matched
  // nothing, the request was never delayed, and the spec failed reporting "no
  // skeleton" about a page that had simply already loaded. A route that does
  // not match is silent; counting hits is the only way to see it.
  let intercepted = 0;
  await page.route(/\/__zeroship\/v1\/cc\.list/, async (route) => {
    intercepted += 1;
    await new Promise((resolve) => setTimeout(resolve, 5000));
    await route.continue();
  });

  await page.goto(`/issues/${issue.id}`);

  const cc = page.locator(".rail-disclosure", { hasText: "CC" }).first();
  await expect(cc, "the group renders before its data arrives").toBeVisible();
  // Greater-than-zero, not exactly one: React's dev-mode double-invoked
  // effects fire the query twice, and pinning the count would make this spec
  // fail on a detail it is not about.
  expect(
    intercepted,
    "the CC request was actually held open, so this run tests the loading state",
  ).toBeGreaterThan(0);
  await expect(
    cc.locator("[data-slot~=\"skeleton\"]"),
    "an unanswered CC list shows a placeholder, not a claim that nobody is on it",
  ).toBeVisible();

  // ONE read of both facts. Asserting them as two auto-retrying expects is
  // what the first version did, and it failed: the skeleton check passed while
  // loading, the 2s answer landed, and the "no --" check then ran against the
  // resolved -- and correctly empty -- list. Two true statements about
  // different instants do not compose into a statement about one instant.
  const snapshot = await cc.evaluate((el) => ({
    hasSkeleton: el.querySelector("[data-slot~=\"skeleton\"]") !== null,
    text: (el as HTMLElement).innerText,
  }));
  expect(snapshot.hasSkeleton, "still loading at the moment of the read").toBe(true);
  expect(
    snapshot.text,
    "a loading group does not print the absent token, which would be a claim about the issue",
  ).not.toContain("--");
});
