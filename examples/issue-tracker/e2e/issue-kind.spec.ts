import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { chooseOption } from "./select";
import { signIn } from "./session";

/**
 * An issue says WHAT it is separately from HOW BAD it is.
 *
 * This is the change the whole rename exists for. Bugzilla spells a feature
 * request `severity: enhancement`, so one field has to answer either what a row
 * is or how much it hurts -- and "a critical feature request" is therefore
 * unsayable, while every severity distribution is polluted by rows that are not
 * broken at all. bugzilla.mozilla.org dropped that spelling for a Type field,
 * and this app follows it: `kind` is defect/enhancement/task, `severity` is
 * impact only, and `enhancement` is gone from the severity vocabulary.
 *
 * Both tests below turn on a matched pair differing in ONE variable. The filing
 * test files an enhancement AT severity critical, so a create path that still
 * folded the two together -- writing the kind into severity, or refusing the
 * combination -- cannot produce this row. The filter test seeds two issues that
 * are identical except for their kind, both `severity: critical`, so a filter
 * that answered on severity, or one that matched everything, fails on the half
 * that must NOT come back.
 *
 * WHAT THIS DOES NOT CATCH. It drives `enhancement` only: `task` is in
 * ISSUE_KINDS and nothing here files one, so a kind that is offered but not
 * persisted would survive this. It asserts the severity vocabulary from the
 * FILING form alone -- the rail's own severity control is a separate list, so
 * an `enhancement` left behind there is invisible here. And it says nothing
 * about editing kind after the fact (`issues.setKind`), only about filing.
 *
 * These are scope statements, not mutation measurements: the specs were written
 * against a database mid-rename and have not been run.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test.beforeEach(async ({ context, baseURL }) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });
});

test("a feature request is filed as an enhancement and stays critical", async ({
  page,
  baseURL,
}) => {
  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    key: productKey("KIND"),
    name: `Kinds ${RUN}`,
    description: "kind spec",
  });
  await rpc("components.create", { productId: product.id, name: "Core", description: "core" });
  // A version EXISTS on this product and is deliberately not chosen. "Version
  // found in" is a defect concept, `issues.versionId` is nullable for that
  // reason, and a form that still required one would make a feature request
  // name a release it has nothing to do with.
  await rpc("versions.create", { productId: product.id, name: "1.0" });

  const summary = `Export to CSV ${RUN}`;

  await page.goto("/issues/new");
  await chooseOption(page, page, "Product", `Kinds ${RUN}`);
  await chooseOption(page, page, "Component", "Core");
  await page.getByLabel("Summary").fill(summary);
  await page.getByLabel("Description").fill("people keep asking for this");

  await chooseOption(page, page, "Kind", "enhancement");

  // The severity list, opened by hand rather than through chooseOption, because
  // what it OFFERS is half the claim. `critical` is the control: without it a
  // severity select that failed to render would satisfy the absence check below
  // on its own.
  const severityTrigger = page.getByRole("combobox", { name: "Severity" });
  await severityTrigger.click();
  const severities = page.getByRole("listbox");
  await expect(
    severities.getByRole("option", { name: "critical", exact: true }),
    "the severity list rendered, so the absence below is about the vocabulary",
  ).toBeVisible();
  await expect(
    severities.getByRole("option", { name: "enhancement", exact: true }),
    "severity no longer offers enhancement; that question is answered by kind",
  ).toHaveCount(0);
  await severities.getByRole("option", { name: "critical", exact: true }).click();

  await page.getByRole("button", { name: "File issue" }).click();

  // Filed at all, with no version. The id prefix is `issu_` because the
  // platform derives it from the collection name (`issues` -> strip the
  // trailing s -> first four alphanumerics), so this also pins that the URL
  // carries a real minted id rather than an empty segment.
  await expect(page).toHaveURL(/\/issues\/issu_[A-Za-z0-9]{12,}/);

  // Both facts, on screen, as two independent rail rows.
  await expect(
    page.locator(".rail-choice", { hasText: "Kind" }),
    "the rail states what this issue is",
  ).toContainText("enhancement");
  await expect(
    page.locator(".rail-choice", { hasText: "Severity" }),
    "and how bad it is, without one answer overwriting the other",
  ).toContainText("critical");

  // And the same pair as STORED, not merely as rendered from the form state
  // still in memory. A reload would prove it too; reading the row back over RPC
  // says which value the server holds.
  const id = new URL(page.url()).pathname.split("/").filter(Boolean).pop()!;
  const stored = await rpc("issues.get", { id });
  expect(stored.issue.kind, "the kind reached the server").toBe("enhancement");
  expect(stored.issue.severity, "and so did the severity, unchanged by it").toBe("critical");
  expect(
    stored.issue.versionId ?? null,
    "a feature request is filed without a version it was found in",
  ).toBeNull();
});

test("the issue list filters by kind, and the filter is not severity in disguise", async ({
  page,
  baseURL,
}) => {
  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    key: productKey("FILT"),
    name: `Filter ${RUN}`,
    description: "kind filter spec",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });

  // The two rows differ in kind and in NOTHING else -- same product, same
  // component, same severity, same run marker. Any behaviour that separates
  // them is behaviour that read `kind`.
  const file = (summary: string, kind: string) =>
    rpc("issues.create", {
      productId: product.id,
      componentId: component.id,
      summary,
      description: "d",
      kind,
      severity: "critical",
    });
  const wanted = `Dark mode ${RUN}`;
  const unwanted = `Crash on save ${RUN}`;
  await file(wanted, "enhancement");
  await file(unwanted, "defect");

  await page.goto("/issues");

  // Narrow to this run's product first, so the assertions are about these two
  // rows rather than about whatever else the dev database holds.
  const productFilter = page.getByRole("combobox", { name: /product/i }).first();
  await productFilter.click();
  await page.getByRole("option", { name: `Filter ${RUN}`, exact: true }).click();
  await expect(page.getByText(wanted), "both rows are in scope before filtering").toBeVisible();
  await expect(page.getByText(unwanted), "including the defect").toBeVisible();

  const kindFilter = page.getByRole("combobox", { name: /kind/i }).first();
  await kindFilter.click();
  await page.getByRole("option", { name: "enhancement", exact: true }).click();

  await expect(page.getByText(wanted), "the enhancement survives the kind filter").toBeVisible();
  await expect(
    page.getByText(unwanted),
    "and the defect does not, though it has the same severity",
  ).toHaveCount(0);
});
