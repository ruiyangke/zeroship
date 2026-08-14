import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * The rail stays put, and does not draw through itself.
 *
 * Measured on a 1440x800 laptop with a twelve-comment thread: the shell's
 * scrollport is 751px and the rail is 1071px, so reading to the end of the
 * conversation put the rail's top at -608px. Status, resolution, severity,
 * priority and assignee were all off screen. State anchored at the top of the
 * page is only anchored until you read anything.
 *
 * The second assertion is the one worth having. Capping the rail's height made
 * its groups -- flex items, so flex-shrink:1 by default -- squeeze below their
 * content, which then overflowed and painted OVER the group beneath it. Every
 * measurement still read correct: the rail was on screen, at the right height,
 * sticky. Only a screenshot showed LINKS drawn through WHERE. So this asserts
 * geometry between siblings, which is the thing a number-based check missed.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("the rail holds its place and its groups do not overlap", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Sticky ${RUN}`,
    key: productKey("STK"),
    description: "sticky",
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
    summary: `Sticky rail ${RUN}`,
    description: "seed",
  });
  // Long enough that the thread outruns the viewport, which is the whole case.
  for (let i = 0; i < 12; i++) {
    await rpc("comments.add", { bugId: bug.id, body: `Comment number ${i} with a sentence.` });
  }

  // A laptop, not a tall test window -- the bug only exists when the rail is
  // taller than the scrollport.
  await page.setViewportSize({ width: 1440, height: 800 });
  await page.goto(`/bugs/${bug.id}`);
  await expect(page.locator("ul.comment-list")).toBeVisible();

  // The page itself does not scroll; the shell's main does. Scrolling the
  // window here would move nothing and the test would pass without testing.
  const scrolled = await page.evaluate(() => {
    const main = document.querySelector("main.zs-app-shell__main") as HTMLElement | null;
    if (!main || main.scrollHeight <= main.clientHeight + 40) return false;
    main.scrollTop = main.scrollHeight;
    return true;
  });
  expect(scrolled, "the thread is long enough to scroll, so the case is real").toBe(true);
  await page.waitForTimeout(400);

  // 1. STILL THERE. The status control is the top of the rail.
  const status = page.getByRole("combobox", { name: /status/i }).first();
  const onScreen = await status.evaluate((el) => {
    const r = el.getBoundingClientRect();
    return r.top >= 0 && r.bottom <= window.innerHeight;
  });
  expect(onScreen, "status is still readable after scrolling to the end of the thread").toBe(true);

  // 2. NOT DRAWN THROUGH ITSELF.
  const overlaps = await page.evaluate(() => {
    const rows = Array.from(
      document.querySelectorAll(".bug-detail-side .rail-choice, .bug-detail-side .rail-section"),
    ) as HTMLElement[];
    const boxes = rows.map((el) => ({
      label: (el.textContent ?? "").trim().slice(0, 24),
      ...el.getBoundingClientRect().toJSON(),
    }));
    const bad: string[] = [];
    for (let i = 0; i < boxes.length; i++) {
      for (let j = i + 1; j < boxes.length; j++) {
        const a = boxes[i];
        const b = boxes[j];
        // Same column, so any vertical intersection is one drawn over another.
        const overlapY = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
        if (overlapY > 2) bad.push(`"${a.label}" over "${b.label}" by ${Math.round(overlapY)}px`);
      }
    }
    return bad.slice(0, 6);
  });
  expect(overlaps, `rail groups are painted on top of each other:\n${overlaps.join("\n")}`).toEqual(
    [],
  );
});
