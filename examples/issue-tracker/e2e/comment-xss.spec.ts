import { expect, test } from "@playwright/test";

import { signIn } from "./session";
import { productKey } from "./keys";

/**
 * Stored comment HTML cannot execute.
 *
 * `comments.add` takes a body string over RPC, so a caller can store whatever
 * markup they like without ever touching the editor -- the toolbar is not a
 * filter. Every reader of the bug then renders that markup. This is the stored
 * XSS path, and until now the only thing standing behind it was a claim in a
 * code comment.
 *
 * The defence is that rendering goes through a read-only tiptap editor rather
 * than dangerouslySetInnerHTML, so the stored HTML is re-parsed by the same
 * ProseMirror schema that writes it: a tag the schema does not define is
 * dropped rather than rendered, and `<a href>` is additionally checked against
 * the Link extension's protocol allowlist in parseHTML (http/https/ftp/ftps/
 * mailto/tel/callto/sms/cid/xmpp -- javascript: is absent).
 *
 * WHAT THIS DOES NOT COVER: it pins the payloads below, not the schema. Widen
 * EXTENSIONS in RichText.tsx -- add raw HTML passthrough, an iframe node, a
 * node with a URL attribute other than Link's href -- and this test keeps
 * passing while the surface it guards has moved. Adding a node is a security
 * decision; this file is not what will tell you so.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

const PAYLOADS = [
  `<script>window.__xss = "script tag"</script>`,
  `<img src=x onerror='window.__xss = "img onerror"'>`,
  `<a href="javascript:window.__xss='js href'">click me</a>`,
  `<p onclick='window.__xss = "inline handler"'>inert paragraph</p>`,
  `<iframe src="javascript:window.__xss='iframe'"></iframe>`,
  // Whitespace-obfuscated scheme: the allowlist strips unicode whitespace
  // before matching, so this must not resurrect the javascript: case.
  `<a href="java&#09;script:window.__xss='obfuscated'">obfuscated</a>`,
];

test("stored comment markup cannot execute", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Xss ${RUN}`,
    key: productKey("XSS"),
    description: "xss",
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
    summary: `Hostile markup ${RUN}`,
    description: "<p>seed</p>",
  });

  for (const body of PAYLOADS) {
    await rpc("comments.add", { bugId: bug.id, body });
  }
  // The positive control. Without it, a renderer that silently dropped EVERY
  // comment would pass every assertion below while being entirely broken --
  // "nothing executed" and "nothing rendered" look identical otherwise.
  await rpc("comments.add", {
    bugId: bug.id,
    body: `<p>benign <strong>bold</strong> and <a href="https://example.com/safe">a safe link</a></p>`,
  });

  const executed: string[] = [];
  page.on("dialog", async (dialog) => {
    executed.push(`dialog:${dialog.message()}`);
    await dialog.dismiss();
  });

  await page.goto(`/#/bugs/${bug.id}`);
  await expect(page.locator("li.comment")).toHaveCount(PAYLOADS.length + 2);

  // The control first: if this fails, the rest proves nothing.
  const safeLink = page.locator('li.comment a[href="https://example.com/safe"]');
  await expect(safeLink, "a legitimate link still renders").toHaveCount(1);
  await expect(
    page.locator("li.comment strong", { hasText: "bold" }),
    "legitimate formatting still renders",
  ).toHaveCount(1);

  // Nothing ran.
  const flag = await page.evaluate(() => (window as unknown as { __xss?: string }).__xss);
  expect(flag, "no payload set the execution flag").toBeUndefined();
  expect(executed, "no payload opened a dialog").toEqual([]);

  // And nothing dangerous reached the DOM in the first place.
  const dom = await page.evaluate(() => {
    const list = document.querySelector("ul.comment-list");
    if (!list) return null;
    const withHandlers = Array.from(list.querySelectorAll("*")).filter((el) =>
      Array.from(el.attributes).some((a) => a.name.startsWith("on")),
    ).length;
    const hrefs = Array.from(list.querySelectorAll("a")).map((a) => a.getAttribute("href") ?? "");
    return {
      scripts: list.querySelectorAll("script").length,
      iframes: list.querySelectorAll("iframe").length,
      images: list.querySelectorAll("img").length,
      withHandlers,
      hrefs,
    };
  });

  expect(dom, "the comment list rendered").not.toBeNull();
  expect(dom!.scripts, "no script element survived").toBe(0);
  expect(dom!.iframes, "no iframe survived").toBe(0);
  expect(dom!.images, "no img survived -- the schema has no image node").toBe(0);
  expect(dom!.withHandlers, "no on* handler attribute survived").toBe(0);
  for (const href of dom!.hrefs) {
    expect(href.replace(/\s/g, "").toLowerCase(), `href ${href} is not a script url`).not.toContain(
      "javascript:",
    );
  }
});
