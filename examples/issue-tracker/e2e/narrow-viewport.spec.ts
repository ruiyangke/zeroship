import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * Nothing on the issue page runs off a narrow screen.
 *
 * Every other spec and every screenshot in this suite ran at 1280px or wider,
 * so the phone width was simply never looked at -- and the fold's own label,
 * "Show flags, votes and security", ran past the
 * right edge and was clipped. Buttons do not wrap by default, so the one thing
 * that label exists to tell you was the part cut off.
 *
 * The check is for elements extending past the viewport rather than for body
 * scroll: the page did NOT scroll sideways, because the overflow was clipped
 * instead. "No horizontal scrollbar" would have passed while the control was
 * unreadable, which is how this survived so long.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test.use({ viewport: { width: 390, height: 1400 } });

test("the issue page fits a phone", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Phone ${RUN}`,
    key: productKey("PHN"),
    description: "phone",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Uploads",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const issue = await rpc("issues.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: "Multipart upload stalls at 99 percent for objects over 2 GB",
    description: "The progress bar reaches 99% and never completes.",
  });

  await page.goto(`/issues/${issue.id}`);
  await expect(page.locator("li.comment").first()).toBeVisible();

  const overflowing = await page.evaluate(() => {
    const limit = document.documentElement.clientWidth;
    return Array.from(document.querySelectorAll<HTMLElement>(".page *"))
      .filter((el) => {
        const box = el.getBoundingClientRect();
        // Zero-size nodes cannot be seen to overflow.
        return box.width > 0 && box.right > limit + 1;
      })
      .map((el) => el.tagName.toLowerCase() + "." + String(el.className).split(" ")[0])
      .slice(0, 8);
  });

  expect(overflowing, "nothing extends past the right edge of a phone").toEqual([]);

  // The fold's label is the specific thing that was clipped, so it is named:
  // a generic overflow check would go green if the button were removed.
  const toggle = page.getByRole("button", { name: /flags, votes and security/i });
  await expect(toggle, "the fold is still offered").toBeVisible();
  const fits = await toggle.evaluate(
    (el) => el.getBoundingClientRect().right <= document.documentElement.clientWidth + 1,
  );
  expect(fits, "and its label fits the screen rather than being cut off").toBe(true);
});
