import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * A product can be removed, and says what that costs before it does it.
 *
 * Products administration could create and rename and never delete, so a
 * product created by mistake stayed in the filing dropdown forever. Bugzilla
 * has this; it was a gap here rather than a documented non-goal.
 *
 * The refusal is the interesting half. A product almost always holds issues,
 * and deleting it takes their comments, attachments and history too -- so the
 * default answer is a 409 carrying the count, and the caller has to pass
 * `deleteIssues` to mean it. A destructive operation that just works on the
 * first call is one nobody can undo a mistake with.
 *
 * WHAT THIS DOES NOT CATCH. It checks the API contract and the rows, not the
 * admin UI. It also cannot see orphaned rows through the API at all -- a
 * comment whose issue is gone simply stops being reachable, which is why the
 * cascade was verified once directly against SQLite (ten left-join checks, all
 * zero) rather than inferred from a 404 here. This spec guards the behaviour
 * that IS observable: the refusal, the counts, and that the issue and product
 * both stop resolving.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("deleting a product refuses until the issue count is confirmed", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const call = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    return { status: res.status(), body: (await res.json()) as Record<string, never> };
  };
  const rpc = async (proc: string, json: unknown) => {
    const r = await call(proc, json);
    expect(r.status, `${proc} should succeed`).toBe(200);
    return (r.body as { json: never }).json;
  };

  const product = await rpc("products.create", {
    name: `Doomed ${RUN}`,
    key: productKey("DOO"),
    description: "doomed",
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
    summary: `Doomed issue ${RUN}`,
    description: "d",
  });
  await rpc("comments.add", { issueId: issue.id, body: "and a comment that goes with it" });

  // 1. The refusal names the cost rather than just saying no.
  const refused = await call("products.delete", { id: product.id });
  expect(refused.status, "a product with issues is not deleted on the first ask").toBe(409);
  expect(
    (refused.body as unknown as { message: string }).message,
    "and the refusal says how many issues would go with it, so the confirmation is informed",
  ).toMatch(/holds 1 issue\b/);

  // Control: the refusal was a refusal, not a silent success.
  expect(
    (await call("products.get", { id: product.id })).status,
    "the product is still there after being refused",
  ).toBe(200);

  // 2. Confirmed, it reports what it took.
  const deleted = await rpc("products.delete", { id: product.id, deleteIssues: true });
  expect(deleted).toMatchObject({ deleted: true, issuesDeleted: 1 });

  // 3. Both the product and its issue stop resolving.
  expect((await call("products.get", { id: product.id })).status).toBe(404);
  expect(
    (await call("issues.get", { id: issue.id })).status,
    "the issue went with its product rather than being left pointing at nothing",
  ).toBe(404);
});

test("an empty product deletes without confirmation", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const call = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    return { status: res.status(), body: (await res.json()) as Record<string, never> };
  };

  // The counterpart to the refusal above. Without this, a gate that refused
  // EVERYTHING would pass the first test just as well.
  const created = await call("products.create", {
    name: `Empty ${RUN}`,
    key: productKey("EMP"),
    description: "empty",
  });
  expect(created.status).toBe(200);
  const product = (created.body as unknown as { json: { id: string } }).json;

  const deleted = await call("products.delete", { id: product.id });
  expect(deleted.status, "nothing would be lost, so nothing needs confirming").toBe(200);
  expect((deleted.body as unknown as { json: { issuesDeleted: number } }).json.issuesDeleted).toBe(0);
});
