import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * Files are attached BY commenting, and appear with the comment they came on.
 *
 * They used to be a panel on the far side of the page from the comment box, so
 * posting "here is the trace" and posting the trace were two errands with
 * nothing joining them -- the file landed in a list with a filename and no
 * explanation, which is how attachments end up meaning nothing a month later.
 *
 * The attachment is uploaded through the real picker, not over RPC, because
 * the thing under test is that the composer wires the two together: the
 * comment must exist before a file can name it, and the spec would pass
 * against a broken composer if it did the upload itself.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

/**
 * This spec was marked `test.fail()` for a day against a platform bug that
 * does not exist. Worth the paragraph, because the false bug was reproducible.
 *
 * `attachments.get` really did return a row with no `commentId` key while
 * SQLite held a value for it, and `find({commentId})` really did match that
 * row -- filter sees the column, projection does not. Every artifact agreed
 * the column was declared: the table, the migration, the generated schema.
 *
 * The projection is built from the schema the RUNNING isolate registered
 * (`implicit_read_projection_parts`, crates/zeroship-schema/src/query.rs), and
 * this runtime process had booted two seconds BEFORE the commit that added the
 * column. It was serving a schema that predated it. Nothing was stale on disk,
 * which is why re-reading the files kept confirming the bug.
 *
 * Dev is supposed to survive that: a migration edit regenerates the descriptor,
 * ships it on the next HMR poll, and calls `resetSchemaInstalled()` so the next
 * request re-registers (sdks/vite-plugin/src/dev-bootstrap/index.ts). That path
 * is wired and reads correctly. Why it did not take effect here is NOT known --
 * no reproduction, so no claim.
 *
 * The tell was in the same response and I read past it twice: `issues.get`
 * returned two junk keys named with literal quote marks, `"\"estimatedTime
 * Minutes\"": "estimatedTimeMinutes"`, for columns dropped an hour earlier.
 * That is SQLite's double-quoted-string fallback -- an unresolvable "ident"
 * degrades to a string literal -- so the projection was naming columns the
 * table did not have AND missing one it did. One cause, both directions.
 *
 * Restarting the runtime fixed both. If a read ever disagrees with the schema
 * again, restart before you go into the Rust: a live process is the one input
 * that grep cannot check.
 */
test("a file attached while commenting appears with that comment", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Attach ${RUN}`,
    key: productKey("ATC"),
    description: "attach",
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
    summary: `Attach while commenting ${RUN}`,
    description: "seed",
  });

  await page.goto(`/issues/${issue.id}`);

  const editor = page.getByLabel("Add a comment");
  await expect(editor).toBeVisible();
  await editor.click();
  await page.keyboard.type("Here is the trace.");

  await page.getByLabel("Attach files to this comment").setInputFiles({
    name: "trace.log",
    mimeType: "text/plain",
    buffer: Buffer.from("boom\nboom\n"),
  });
  // The queue names what is about to go, so nothing is uploaded silently.
  await expect(page.getByText("trace.log")).toBeVisible();

  await page.getByRole("button", { name: "Comment", exact: true }).click();

  // The file is listed INSIDE its comment, not in a separate panel.
  const posted = page.locator("li.comment", { hasText: "Here is the trace." });
  await expect(posted, "the comment posted").toHaveCount(1);
  await expect(
    posted.locator(".comment-attachments"),
    "and its file is listed with it",
  ).toContainText("trace.log");

  // The server really did associate the two, rather than the page only
  // appearing to: a rendering that grouped by upload order would look the
  // same and be wrong the moment a second comment arrived.
  const files = (await rpc("attachments.list", { issueId: issue.id })) as Array<{
    filename: string;
    commentId: string | null;
  }>;
  const stored = files.find((f) => f.filename === "trace.log");
  expect(stored, "the file was stored").toBeTruthy();
  expect(stored!.commentId, "and it names the comment it arrived with").toBeTruthy();

  // The roll-up still lists it, because "what is attached to this issue" is a
  // question the thread cannot answer at a glance.
  await expect(page.locator("section.attachments-panel")).toContainText("trace.log");
});
