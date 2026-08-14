import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * A group can be deleted -- but not while it still restricts something.
 *
 * `groups.create` existed with no counterpart, so a tracker accumulated groups
 * permanently: this app's own dev database reached 95, all of them leftover
 * fixtures, on a page whose subject is products. The create button was a
 * one-way door.
 *
 * The refusal is the half worth guarding. Deleting a group that still carries
 * `bugGroups` or `productGroups` rows does not just remove a row -- it makes
 * every bug behind it readable by everyone, at the exact moment an admin is
 * tidying up and least expects a visibility change. So the two arms below are
 * a matched pair differing in ONE variable: whether the group restricts a bug.
 * The delete-succeeds arm alone would pass against a server that never checks;
 * the refusal arm alone would pass against one that never deletes.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("a group deletes when unused and refuses while it restricts a bug", async ({
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
    name: `Del ${RUN}`,
    key: productKey("DEL"),
    description: "deletion",
  });
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
    summary: `Guarded ${RUN}`,
    description: "restricted",
  });

  const unusedName = `unused-${RUN}`;
  const inUseName = `in-use-${RUN}`;
  await rpc("groups.create", { name: unusedName, description: "nothing uses this" });
  const inUse = await rpc("groups.create", { name: inUseName, description: "restricts a bug" });
  await rpc("bugs.restrict", { bugId: bug.id, groupId: inUse.id });

  await page.goto("/products");
  const admin = page.locator("section.groups-admin");
  const list = admin.locator("ul.group-list");
  await expect(list).toContainText(unusedName);
  await expect(list).toContainText(inUseName);

  // Arm 1 -- refused, and the reason is shown rather than swallowed.
  await admin.getByRole("button", { name: `Delete group ${inUseName}` }).click();
  await expect(
    admin.locator("p.field-error"),
    "the refusal names what still depends on the group",
  ).toContainText(/still restricts 1 bug/i);
  await expect(list, "and the group is still there").toContainText(inUseName);

  // The restriction genuinely survived the refused delete, evidenced by the
  // server refusing again for the same stated reason -- it can only count
  // "1 bug" if the bugGroups row is still there. Asserting on the bug itself
  // would be more direct, but nothing exposes a bug's restrictions over RPC,
  // and inventing a procedure to make a test easier is the wrong direction.
  const second = await page.request.post(`${baseURL}/__zeroship/v1/groups.delete`, {
    data: { json: { id: inUse.id } },
  });
  expect(second.status(), "a second delete is refused too").toBe(409);
  expect(JSON.stringify(await second.json())).toMatch(/still restricts 1 bug/i);

  // Arm 2 -- the one-variable partner: same button, same list, a group that
  // restricts nothing.
  await admin.getByRole("button", { name: `Delete group ${unusedName}` }).click();
  await expect(list, "an unused group is deleted").not.toContainText(unusedName);
  await expect(list, "and only that one").toContainText(inUseName);

  // The name is reusable afterwards, which a soft delete would prevent: the
  // tombstone keeps the unique index occupied and groups.create rejects it.
  const recreated = await rpc("groups.create", { name: unusedName, description: "again" });
  expect(recreated.id, "the deleted group's name can be used again").toBeTruthy();
  await rpc("groups.delete", { id: recreated.id });

  // Clean up the in-use group too, now that there is a way to. Specs creating
  // groups and never removing them is how the dev database reached 95 of them
  // on a page about products -- and this spec, of all of them, has no excuse.
  // Unrestrict first: the server refuses while the group is load-bearing,
  // which is the whole point of it.
  await rpc("bugs.unrestrict", { bugId: bug.id, groupId: inUse.id });
  await rpc("groups.delete", { id: inUse.id });
});
