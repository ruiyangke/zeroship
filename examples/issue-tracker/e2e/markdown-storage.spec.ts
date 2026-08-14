import { expect, test } from "@playwright/test";

import { signIn } from "./session";
import { productKey } from "./keys";

/**
 * Comment bodies are STORED as markdown, and still render as rich text.
 *
 * Both halves matter and neither implies the other. Rendering alone would look
 * identical if the editor were still serialising HTML -- the page cannot tell
 * you what went into the database. So this reads the stored row back over RPC
 * and asserts on the bytes, then asserts the same body renders as real markup
 * rather than as visible asterisks.
 *
 * The point of the format is that the stored value is legible outside this app:
 * in a notification email, in a grep of the table, to a CLI or an agent filing
 * an issue over `comments.add`.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("bodies are stored as markdown and render as rich text", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Md ${RUN}`,
    key: productKey("MD"),
    description: "markdown",
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
    summary: `Markdown storage ${RUN}`,
    description: "seed",
  });

  await page.goto(`/issues/${issue.id}`);
  const editor = page.getByLabel("Add a comment");
  await expect(editor).toBeVisible();

  // Compose with the toolbar so the editor -- not the test -- decides the
  // serialisation. Typing raw "**bold**" would prove nothing: the assertion
  // below would pass on a string that merely round-tripped unchanged.
  await editor.click();
  await page.keyboard.type("plain then ");
  await page.getByRole("button", { name: "Bold" }).click();
  await page.keyboard.type("emphasised");

  await page.getByRole("button", { name: "Comment", exact: true }).click();
  await expect(page.locator("li.comment")).toHaveCount(2);

  // What actually landed in the database.
  const comments = (await rpc("comments.list", { issueId: issue.id })) as Array<{
    commentNumber: number;
    body: string;
  }>;
  const posted = comments.find((c) => c.commentNumber === 1);
  expect(posted, "the comment was stored").toBeTruthy();
  expect(posted!.body, "stored as markdown emphasis").toContain("**emphasised**");
  expect(posted!.body, "not stored as HTML").not.toContain("<strong>");
  expect(posted!.body, "not stored as HTML").not.toContain("<p>");

  // And the same stored value renders as markup, not as literal asterisks.
  const rendered = page.locator("li.comment").last();
  await expect(rendered.locator("strong"), "markdown renders as real emphasis").toHaveText(
    "emphasised",
  );
  await expect(rendered, "the asterisks are not shown to the reader").not.toContainText("**");
});
