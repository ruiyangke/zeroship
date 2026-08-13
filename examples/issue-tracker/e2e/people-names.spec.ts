import { expect, test } from "@playwright/test";

import { devUser, signIn } from "./session";
import { productKey } from "./keys";

/**
 * A bug names people. It has to name them the way a person is named.
 *
 * The detail page printed `user_0345pl8prFezDsK4tQtOTB` for the assignee, the
 * reporter and every comment author, while the CC panel a few pixels to the
 * right showed "Alice Dev" -- `cc.list` resolved its user and no other
 * endpoint did. The page rendered, no request failed, and the ids were
 * correct; it was just unreadable.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

// A typed_id for an app user. Deliberately not anchored to a particular
// prefix length, so it still matches if the id scheme changes shape.
const RAW_USER_ID = /user_[A-Za-z0-9]{10,}/;

test("the bug page names people rather than printing their ids", async ({
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

  const product = await rpc("products.create", { key: productKey("PEOP"),
    name: `People ${RUN}`, description: "people" });
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
    summary: `Named people ${RUN}`,
    description: "the description becomes comment #0, authored by the reporter",
  });
  await rpc("comments.add", { bugId: bug.id, body: `A comment ${RUN}` });

  await page.goto(`/#/bugs/${bug.id}`);
  const main = page.locator(".bug-detail-main");
  await expect(main).toBeVisible();

  // The dev identity's display name, taken from the session rather than
  // hardcoded, so renaming the fixture user cannot leave this asserting on a
  // name nobody has.
  const name = devUser.name;

  // Located by the LABEL and then asserted on its text, rather than matched
  // against the whole string "Reporter: Alice Dev". A text match that misses
  // reports "element(s) not found", which says nothing about what the page
  // actually rendered; this way the failure quotes the id it found instead.
  // The read-only facts are a description list now, so the reporter is a
  // <dd> beside a "Reporter" <dt> rather than a "Reporter: name" span.
  const reporter = page
    .locator("div", { has: page.getByText("Reporter", { exact: true }) })
    .last();
  await expect(reporter, "the reporter is a person").toContainText(name);
  const assignee = page.locator(".field-block", { hasText: "Assignee" }).first();
  await expect(assignee, "the assignee is a person").toContainText(name);

  const authors = page.locator(".comment-author");
  await expect(authors).toHaveCount(2); // the description is comment #0
  for (const author of await authors.all()) {
    await expect(author, "every comment is attributed to a person").toHaveText(name);
  }

  // The paired half. Showing the name somewhere is not the same as showing it
  // INSTEAD of the id: an earlier version of this page rendered both, and an
  // assertion that only looked for the name would have passed on it.
  const body = (await main.textContent()) ?? "";
  expect(
    body.match(RAW_USER_ID)?.[0] ?? null,
    "no raw user id should survive anywhere in the bug body",
  ).toBeNull();
});
