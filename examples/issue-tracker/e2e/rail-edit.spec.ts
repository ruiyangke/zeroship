import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * The rail states properties, and becomes editable only when asked.
 *
 * It used to render a bordered select per property, so a column whose job is
 * to state facts about the bug was a column of form controls -- heavier on the
 * page than the conversation beside it. Now the value shows and a control
 * appears on demand.
 *
 * The risk this guards is specific: a read-first control can become a control
 * you cannot reach. So the spec drives it by FOCUS, not by forcing a click on
 * a hidden element, and then checks the change actually reached the server --
 * a widget that opens, closes and saves nothing would look identical.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("a rail property reads as a value and edits in place", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Rail ${RUN}`,
    key: productKey("RAI"),
    description: "rail",
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
    summary: `Rail edit ${RUN}`,
    description: "seed",
  });

  await page.goto(`/bugs/${bug.id}`);

  const row = page.locator(".rail-choice", { hasText: "Severity" });
  await expect(row, "the property states its value").toContainText("normal");
  await expect(
    row.getByRole("combobox"),
    "and no control is rendered until one is wanted",
  ).toHaveCount(0);

  // Reachable without a mouse: focusing the affordance is what reveals it.
  const edit = page.getByRole("button", { name: "Edit Severity" });
  await edit.focus();
  await expect(edit, "the way in appears on focus, not only on hover").toBeVisible();
  await edit.click();

  const select = row.getByRole("combobox");
  await expect(select, "the control appears in place").toBeVisible();
  await select.click();
  await page.getByRole("option", { name: "critical", exact: true }).click();

  // Back to a value, showing the new one.
  await expect(row, "the row states the new value").toContainText("critical");
  await expect(row.getByRole("combobox"), "and the control goes away again").toHaveCount(0);

  // It really saved. A control that opens, closes and writes nothing looks
  // exactly the same from the outside.
  const stored = await rpc("bugs.get", { id: bug.id });
  expect(stored.bug.severity, "the change reached the server").toBe("critical");
});

/**
 * The editor has to fit the rail it opens in.
 *
 * Assignee edits with a people search rather than a list, and a search field
 * plus its button does not fit beside a label in a column sized for the word
 * "major". Dropped in unchanged, the input kept its intrinsic width -- a flex
 * item will not shrink below its content unless told to -- and pushed the
 * Find button off the right edge of the page.
 *
 * Checked at desktop width on purpose. The phone-width spec cannot see this:
 * the rail is full width there, so the row that overflows at 1440 has room.
 */
test("the assignee editor stays inside the rail", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Fit ${RUN}`,
    key: productKey("FIT"),
    description: "fit",
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
    summary: `Assignee editor fits ${RUN}`,
    description: "seed",
  });

  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.goto(`/bugs/${bug.id}`);

  await page.getByRole("button", { name: "Edit Assignee", exact: true }).click();

  const rail = page.locator(".bug-detail-side");
  const row = page.locator(".rail-choice.is-editing .user-picker-row");
  await expect(row, "the picker opened").toBeVisible();

  // Measure the BUTTON, not the row that holds it. The row is a grid item
  // sized to its column, so it reports itself inside the rail while its
  // contents spill out of it -- an earlier version of this check asserted on
  // the row and passed happily against the broken layout.
  const find = row.getByRole("button", { name: "Find" });
  const railBox = await rail.boundingBox();
  const findBox = await find.boundingBox();
  expect(railBox, "the rail is laid out").toBeTruthy();
  expect(findBox, "the button is laid out").toBeTruthy();
  expect(
    Math.round(findBox!.x + findBox!.width),
    "the picker ends inside the rail rather than running off the page",
  ).toBeLessThanOrEqual(Math.round(railBox!.x + railBox!.width));
});
