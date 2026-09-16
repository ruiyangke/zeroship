import { expect, test, type Page } from "@playwright/test";
import { authUrl } from "../helpers";

const publicPages = [
  { name: "login", path: "/login" },
  { name: "signup", path: "/signup" },
  { name: "forgot", path: "/forgot" },
] as const;

async function submitFailedLogin(page: Page): Promise<number> {
  await page.goto(authUrl("/login"));
  const form = page.locator('form[action="/login"]');
  await form.locator('input[name="email"]').fill("missing-authui@example.test");
  await form.locator('input[name="password"]').fill("wrong-password");
  const responsePromise = page.waitForResponse(
    (response) =>
      response.request().method() === "POST" && new URL(response.url()).pathname === "/login",
  );
  await form.getByRole("button", { name: "Sign in" }).click();
  const response = await responsePromise;
  expect(response.status(), "failed login response status").toBe(401);
  await expect(page.locator(".error")).toBeVisible();
  await expect(page.locator(".error")).toHaveText("invalid email or password");
  return response.status();
}

test("failed login error has screen-reader announcement semantics", async ({ page }) => {
  const status = await submitFailedLogin(page);
  const error = page.locator(".error");
  const evidence = {
    status,
    role: await error.getAttribute("role"),
    ariaLive: await error.getAttribute("aria-live"),
    alertCount: await page.getByRole("alert").count(),
  };
  console.log(`A11Y_EVIDENCE login_error ${JSON.stringify(evidence)}`);

  const isAnnounced =
    evidence.role === "alert" || evidence.ariaLive === "assertive" || evidence.ariaLive === "polite";
  expect(isAnnounced, `failed-login announcement evidence: ${JSON.stringify(evidence)}`).toBe(true);
});

test("failed login fields identify and describe their errors", async ({ page }) => {
  const status = await submitFailedLogin(page);
  const form = page.locator('form[action="/login"]');
  const errorId = await page.locator(".error").getAttribute("id");
  const fieldEvidence: Array<{
    name: string | null;
    ariaInvalid: string | null;
    ariaDescribedBy: string | null;
    descriptionTargetsExist: boolean;
    describesError: boolean;
  }> = [];

  for (const field of await form.locator('input:not([type="hidden"])').all()) {
    const ariaDescribedBy = await field.getAttribute("aria-describedby");
    const ids = ariaDescribedBy?.split(/\s+/).filter(Boolean) ?? [];
    const descriptionTargetsExist =
      ids.length > 0 &&
      (await page.evaluate(
        (targetIds) => targetIds.every((id) => document.getElementById(id) !== null),
        ids,
      ));
    fieldEvidence.push({
      name: await field.getAttribute("name"),
      ariaInvalid: await field.getAttribute("aria-invalid"),
      ariaDescribedBy,
      descriptionTargetsExist,
      describesError: errorId !== null && ids.includes(errorId),
    });
  }
  console.log(
    `A11Y_EVIDENCE login_fields ${JSON.stringify({ status, errorId, fields: fieldEvidence })}`,
  );

  expect(
    fieldEvidence.map((field) => field.name),
    "failed login field census",
  ).toEqual(["email", "password"]);
  expect.soft(errorId, "visible error id").toBeTruthy();
  for (const field of fieldEvidence) {
    expect.soft(field.ariaInvalid, `${field.name} aria-invalid`).toBe("true");
    expect.soft(field.descriptionTargetsExist, `${field.name} description target`).toBe(true);
    expect.soft(field.describesError, `${field.name} aria-describedby points to the error`).toBe(
      true,
    );
  }
});

test("public auth pages name controls and use a sane heading order", async ({ page }) => {
  for (const authPage of publicPages) {
    await page.goto(authUrl(authPage.path));
    if (authPage.name === "login") {
      await page.locator("details").evaluate((details) => {
        (details as HTMLDetailsElement).open = true;
      });
    }

    const controls = page.locator(
      'input:not([type="hidden"]):visible, select:visible, textarea:visible, button:visible',
    );
    const controlCount = await controls.count();
    expect(controlCount, `${authPage.name} visible form control count`).toBeGreaterThan(0);
    for (let index = 0; index < controlCount; index += 1) {
      await expect(controls.nth(index), `${authPage.name} control ${index + 1}`).toHaveAccessibleName(
        /\S/,
      );
    }

    await expect(page.locator("h1"), `${authPage.name} h1 count`).toHaveCount(1);
    const headingLevels = await page.locator("h1, h2, h3, h4, h5, h6").evaluateAll((headings) =>
      headings.map((heading) => Number(heading.tagName.slice(1))),
    );
    expect(headingLevels[0], `${authPage.name} starts at h1`).toBe(1);
    for (let index = 1; index < headingLevels.length; index += 1) {
      expect(
        headingLevels[index] - headingLevels[index - 1],
        `${authPage.name} heading level ${index + 1} does not skip`,
      ).toBeLessThanOrEqual(1);
    }

    console.log(
      `A11Y_EVIDENCE ${authPage.name}_structure ${JSON.stringify({
        controlCount,
        namedControls: controlCount,
        h1Count: await page.locator("h1").count(),
        headingLevels,
      })}`,
    );
  }
});

test("public auth pages show a focus indicator on every interactive element", async ({ page }) => {
  for (const authPage of publicPages) {
    await page.goto(authUrl(authPage.path));
    if (authPage.name === "login") {
      await page.locator("details").evaluate((details) => {
        (details as HTMLDetailsElement).open = true;
      });
    }

    const selector = [
      'a[href]:visible',
      'button:not(#authui-focus-sentinel):not(:disabled):visible',
      'input:not([type="hidden"]):not(:disabled):visible',
      "select:not(:disabled):visible",
      "textarea:not(:disabled):visible",
      "summary:visible",
      '[tabindex]:not([tabindex="-1"]):not(#authui-focus-sentinel):visible',
    ].join(", ");
    const expectedElements = await page.locator(selector).evaluateAll((elements) =>
      elements.map((element, index) => {
        const htmlElement = element as HTMLElement;
        htmlElement.dataset.authuiFocusIndex = String(index);
        const style = getComputedStyle(htmlElement);
        return {
          focusIndex: String(index),
          backgroundColor: style.backgroundColor,
          borderColor: style.borderColor,
          borderWidth: style.borderWidth,
          boxShadow: style.boxShadow,
          color: style.color,
        };
      }),
    );
    const expectedCount = expectedElements.length;
    expect(expectedCount, `${authPage.name} interactive element count`).toBeGreaterThan(0);
    const baselines = Object.fromEntries(
      expectedElements.map((element) => [element.focusIndex, element]),
    );

    await page.evaluate(() => {
      document.getElementById("authui-focus-sentinel")?.remove();
      const sentinel = document.createElement("button");
      sentinel.id = "authui-focus-sentinel";
      sentinel.textContent = "focus sentinel";
      document.body.prepend(sentinel);
      sentinel.focus();
    });

    const focusEvidence: Array<{
      focusIndex: string;
      element: string;
      focusVisible: boolean;
      outlineStyle: string;
      outlineColor: string;
      outlineWidth: string;
      boxShadow: string;
      hasIndicator: boolean;
    }> = [];
    for (let index = 0; index < expectedCount; index += 1) {
      await page.keyboard.press("Tab");
      focusEvidence.push(
        await page.evaluate((unfocusedStyles) => {
          const active = document.activeElement as HTMLElement | null;
          if (!active || active === document.body) {
            throw new Error("keyboard focus left the interactive sequence");
          }
          if (active.id === "authui-focus-sentinel") {
            throw new Error("keyboard focus wrapped to the focus sentinel");
          }
          const focusIndex = active.dataset.authuiFocusIndex;
          if (focusIndex === undefined) {
            throw new Error("keyboard focus reached an element outside the measured set");
          }
          const baseline = unfocusedStyles[focusIndex];
          if (!baseline) {
            throw new Error(`missing unfocused styles for focus index ${focusIndex}`);
          }
          const style = getComputedStyle(active);
          const focusVisible = active.matches(":focus-visible");
          const outlineIsTransparent =
            style.outlineColor === "transparent" || style.outlineColor === "rgba(0, 0, 0, 0)";
          const hasOutline =
            style.outlineStyle !== "none" &&
            Number.parseFloat(style.outlineWidth) > 0 &&
            !outlineIsTransparent;
          const hasBoxShadow =
            style.boxShadow !== "none" && style.boxShadow !== baseline.boxShadow;
          const hasStyleChange =
            style.backgroundColor !== baseline.backgroundColor ||
            style.borderColor !== baseline.borderColor ||
            style.borderWidth !== baseline.borderWidth ||
            style.color !== baseline.color;
          return {
            focusIndex,
            element: `${active.tagName.toLowerCase()}${active.getAttribute("name") ? `[name=${active.getAttribute("name")}]` : ""}${active.id ? `#${active.id}` : ""}`,
            focusVisible,
            outlineStyle: style.outlineStyle,
            outlineColor: style.outlineColor,
            outlineWidth: style.outlineWidth,
            boxShadow: style.boxShadow,
            hasIndicator: focusVisible && (hasOutline || hasBoxShadow || hasStyleChange),
          };
        }, baselines),
      );
    }
    await page.evaluate(() => {
      document.getElementById("authui-focus-sentinel")?.remove();
      for (const element of document.querySelectorAll<HTMLElement>("[data-authui-focus-index]")) {
        delete element.dataset.authuiFocusIndex;
      }
    });

    console.log(`A11Y_EVIDENCE ${authPage.name}_focus ${JSON.stringify(focusEvidence)}`);
    expect(focusEvidence, `${authPage.name} keyboard focus count`).toHaveLength(expectedCount);
    expect(
      [...new Set(focusEvidence.map((evidence) => evidence.focusIndex))].sort(),
      `${authPage.name} unique keyboard focus targets`,
    ).toEqual(expectedElements.map((element) => element.focusIndex).sort());
    for (const evidence of focusEvidence) {
      expect(evidence.focusVisible, `${authPage.name} ${evidence.element} matches :focus-visible`).toBe(
        true,
      );
      expect(evidence.hasIndicator, `${authPage.name} ${evidence.element} focus indicator`).toBe(
        true,
      );
    }
  }
});
