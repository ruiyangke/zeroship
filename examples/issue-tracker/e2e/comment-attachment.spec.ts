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
 * KNOWN BROKEN UPSTREAM, 2026-08-14. `test.fail` asserts this DOES fail, so
 * the day the platform is fixed this spec errors with "expected to fail but
 * passed" and someone deletes this block. It is a tracking device, not a mute.
 *
 * env.db drops `attachments.commentId` on every READ while accepting it on
 * write. Narrowed with direct RPC calls:
 *
 *   attachments.upload  -> returns commentId: "comm_0346..."   (persisted)
 *   sqlite               -> trace.log|comm_0346...             (stored)
 *   generated schema     -> attachments: { commentId: t.string() }
 *   attachments.list     -> key ABSENT from the row
 *   attachments.get      -> key ABSENT from .attachment
 *
 * Not nullable-ref in general: bugs.milestoneId is a nullable ref and comes
 * back with its key present. Specific to this column, on both read paths.
 *
 * The app code is correct -- the composer sends commentId and the panel groups
 * on it -- so the user-visible symptom is a file that uploads, appears in
 * FILES, and never appears on the comment it arrived with.
 */
test.fail();
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
  const bug = await rpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Attach while commenting ${RUN}`,
    description: "seed",
  });

  await page.goto(`/bugs/${bug.id}`);

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
  const files = (await rpc("attachments.list", { bugId: bug.id })) as Array<{
    filename: string;
    commentId: string | null;
  }>;
  const stored = files.find((f) => f.filename === "trace.log");
  expect(stored, "the file was stored").toBeTruthy();
  expect(stored!.commentId, "and it names the comment it arrived with").toBeTruthy();

  // The roll-up still lists it, because "what is attached to this bug" is a
  // question the thread cannot answer at a glance.
  await expect(page.locator("section.attachments-panel")).toContainText("trace.log");
});
