import { chromium } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) {
  throw new Error("Set STORYBOOK_URL to the running static Storybook URL.");
}

/*
 * The themes and stories lists are intentionally empty during the Apple-HIG
 * rebuild. They are repopulated as components land: each new component adds
 * its story id here, and each new theme (hig-light, hig-dark, ...) adds an
 * entry. The script reports "no stories to check" rather than failing while
 * the system is empty.
 */
const themes = [
  // { label: "HIG Light", value: "hig-light" },
  // { label: "HIG Dark", value: "hig-dark" },
];
const stories = [
  // "components-base-ui--button-states",
];

if (themes.length === 0 || stories.length === 0) {
  console.log("A11y check skipped — no themes or stories registered yet (HIG rebuild in progress).");
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
