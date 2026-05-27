import { chromium } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) {
  throw new Error("Set STORYBOOK_URL to the running static Storybook URL.");
}

/*
 * Themes and story IDs grow as components land. Each new component
 * appends its story IDs here; each new palette appends a theme entry.
 */
const themes = [
  { label: "Crystal", value: "crystal" },
];
const stories = [
  "components-button--all-styles",
  "components-button--all-sizes",
  "components-button--all-states",
  "components-button--destructive",
  "components-button--destructive-disabled",
  "components-button--loading-destructive",
  "components-button--with-slots",
  "components-button--long-label",
  "components-button--focus-visible",
  "components-button--as-child",
];

if (themes.length === 0 || stories.length === 0) {
  console.log("A11y check skipped — no themes or stories registered yet (rebuild in progress).");
  process.exit(0);
}

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
const page = await context.newPage();
const failures = [];

for (const theme of themes) {
  for (const storyId of stories) {
    const themeGlobal = encodeURIComponent(theme.label);
    const url = `${baseUrl}/iframe.html?id=${storyId}&globals=theme:${themeGlobal}`;
    await page.goto(url, { waitUntil: "networkidle" });
    const results = await new AxeBuilder({ page }).analyze();
    const blockingViolations = results.violations.filter((violation) =>
      violation.impact === "serious" || violation.impact === "critical"
    );
    if (blockingViolations.length > 0) {
      failures.push({ theme: theme.value, storyId, violations: blockingViolations });
    }
  }
}

await context.close();
await browser.close();

if (failures.length > 0) {
  for (const failure of failures) {
    console.error(`A11y violations for ${failure.storyId} (${failure.theme})`);
    for (const violation of failure.violations) {
      console.error(`- ${violation.id}: ${violation.help}`);
    }
  }
  process.exit(1);
}

console.log(`A11y clean for ${stories.length} stories across ${themes.length} themes (no serious/critical violations).`);
