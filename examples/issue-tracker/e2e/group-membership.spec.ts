import { expect, test } from "@playwright/test";

import { chooseOption } from "./select";
import { otherUser, signIn } from "./session";
import { productKey } from "./keys";

/**
 * Group access can be revoked, not only granted.
 *
 * `groups.addMember` and `groups.removeMember` both existed, and nothing
 * between them listed a group's members -- so the app could grant access and
 * never show or take it back. For an access-control surface that is the half
 * that matters: a grant nobody can see is a grant nobody can audit.
 *
 * The membership is checked through its CONSEQUENCE, not just the member list.
 * A row disappearing from a list proves the list changed; only reading the
 * restricted issue proves the access did.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

test("an admin can see who is in a group and remove them", async ({
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

  // Alice writes first so she is the admin; Bob provisions by reading later.
  const product = await rpc("products.create", {
    name: `Member ${RUN}`,
    key: productKey("MEM"),
    description: "membership",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const summary = `Members only ${RUN}`;
  const issue = await rpc("issues.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary,
    description: "restricted",
  });

  const bobContext = await browser.newContext();
  await signIn(bobContext, { runtimePort: RUNTIME_PORT, baseURL: baseURL!, user: otherUser });
  const bob = await bobContext.newPage();
  // Bob touches a write so he exists as an app user and can be found by the
  // people picker. Reading alone does not provision him.
  await bob.request.post(`${baseURL}/__zeroship/v1/users.updatePrefs`, {
    data: { json: { timezone: "UTC" } },
  });

  const group = await rpc("groups.create", { name: `mem-${RUN}`, description: "members" });
  await rpc("issues.restrict", { issueId: issue.id, groupId: group.id });

  // Restricted: Bob cannot read it.
  await bob.goto(`/issues/${issue.id}`);
  await expect(bob.getByText(summary), "a non-member is refused").toHaveCount(0);

  // Grant through the UI.
  await page.goto("/products");
  const admin = page.locator("section.groups-admin");
  await chooseOption(page, admin, "Add member to", `mem-${RUN}`);
  await admin.getByLabel("Find user").fill(otherUser.email);
  await admin.getByRole("button", { name: "Search" }).click();
  await admin.getByRole("button", { name: /^Add$/ }).first().click();

  await expect(admin.locator("ul.member-list"), "the member is listed").toContainText(
    otherUser.name,
  );

  // The grant took effect, which the list alone does not show.
  await bob.reload();
  await expect(bob.locator("h1"), "a member can read it").toContainText(summary);

  // Revoke, and check the consequence again rather than the list.
  await admin.getByRole("button", { name: "Remove" }).first().click();
  await expect(admin.getByText(/nobody is in this group/i)).toBeVisible();

  await bob.reload();
  await expect(bob.getByText(summary), "access is gone once removed").toHaveCount(0);

  await bobContext.close();
});
