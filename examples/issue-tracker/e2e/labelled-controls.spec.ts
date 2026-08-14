import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * Every control a person can reach has a name.
 *
 * Written after a mechanical conversion of 54 buttons and every form control
 * in the app to the design system. That kind of rewrite is exactly where a
 * label quietly stops applying -- a `<label>` wrapper removed because the new
 * component renders its own, an input that used to inherit its name from a
 * parent and now does not -- and none of it fails a test that only clicks
 * things. It found five: the comment box, the comment editor, the keyword
 * field, the dependency field and the attachment file picker.
 *
 * A placeholder is NOT a name. It disappears the moment someone types, and it
 * is not announced as a label.
 *
 * EXCLUDES controls that are not reachable. Base UI's Select renders a hidden
 * form input per combobox, and the popup is a portal that exists whether or
 * not it is open -- counting those reports four "nameless inputs" on the bug
 * list that no one can reach or needs to.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

test("no reachable control is missing an accessible name", async ({ page, baseURL, context }) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };
  const bugs = await rpc("bugs.search", { limit: 1 });
  expect(bugs.length, "the fixture needs a bug to open").toBeGreaterThan(0);

  const pages: [string, string][] = [
    ["bug list", "/bugs"],
    ["new bug", "/bugs/new"],
    ["dashboard", "/dashboard"],
    ["products admin", "/products"],
    ["reports", "/reports"],
    ["bug detail", `/bugs/${bugs[0].id}`],
  ];

  const offenders: string[] = [];
  for (const [label, path] of pages) {
    await page.goto(path);
    await page.waitForTimeout(1200);
    const bad = await page.evaluate(() => {
      const out: string[] = [];
      const sel = "input:not([type=hidden]), textarea, select, button";
      for (const el of Array.from(document.querySelectorAll(sel))) {
        const e = el as HTMLElement;
        // Not rendered, or deliberately out of the tab order (Base UI's
        // internal form input for a Select is both a real input and not
        // something a person interacts with).
        if (e.offsetParent === null) continue;
        if (e.getAttribute("tabindex") === "-1") continue;
        if (e.closest("[aria-hidden=true]")) continue;
        if (e.closest("[role=combobox]") || e.closest("[role=listbox]")) continue;

        const id = e.getAttribute("id");
        const named =
          e.getAttribute("aria-label") ||
          e.getAttribute("aria-labelledby") ||
          e.closest("label") ||
          (id && document.querySelector('label[for="' + id + '"]')) ||
          (e.textContent ?? "").trim();
        if (named) continue;
        out.push(
          e.tagName.toLowerCase() +
            (e.getAttribute("type") ? "[" + e.getAttribute("type") + "]" : "") +
            ' placeholder="' +
            (e.getAttribute("placeholder") ?? "") +
            '"',
        );
      }
      return out;
    });
    for (const entry of bad) offenders.push(`${label}: ${entry}`);
  }

  expect(offenders, `controls with no accessible name:\n${offenders.join("\n")}`).toEqual([]);
});
