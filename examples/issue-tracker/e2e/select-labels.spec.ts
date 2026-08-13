import { expect, test } from "@playwright/test";

import { chooseOption } from "./select";
import { signIn } from "./session";

/**
 * A dropdown shows a NAME when something is chosen and its PLACEHOLDER when
 * nothing is.
 *
 * Both halves have been broken, in opposite directions, one causing the other:
 *
 *  - `@zeroship/ui`'s Select rendered the raw value, so every id-valued
 *    picker displayed `prod_03462DM9KMgFwcl7KD7iQ8` where a product name
 *    belongs. Fixed by adding `renderValue`.
 *  - `renderValue` was then called for the EMPTY value too, so it returned ""
 *    and the trigger went blank -- the form's first control looked like a
 *    broken input rather than an unfilled one.
 *
 * Neither is visible to a test that only drives the control: choosing an
 * option works fine in both states. Only reading what the trigger SAYS
 * catches them, which is what this does.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

test("a picker shows its placeholder when empty and a name when chosen", async ({
  page,
  baseURL,
  context,
}) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const name = `Picker ${RUN}`;
  await rpc("products.create", {
    name,
    key: `PICK${String(Date.now()).slice(-5)}`,
    description: "picker",
  });

  await page.goto("/#/bugs/new");
  const product = page.getByRole("combobox", { name: "1. Product" });

  await expect(product, "an untouched picker shows its placeholder").toHaveText(
    "Select a product",
  );

  await chooseOption(page, page, "1. Product", name);

  // The NAME, not the id. Asserting "not empty" would pass on the id, which
  // is the defect this half exists to catch.
  await expect(product, "a chosen product shows its name").toHaveText(name);
});
