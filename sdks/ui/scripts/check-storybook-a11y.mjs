import { chromium } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) {
  throw new Error("Set STORYBOOK_URL to the running static Storybook URL.");
}

const themes = [
  { label: "Studio", value: "studio" },
  { label: "Atelier", value: "atelier" },
  { label: "Dusk", value: "dusk" },
];
const stories = [
  "components-base-ui--button-states",
  "components-base-ui--field-inputs",
  "components-base-ui--choice-controls",
  "components-base-ui--dialog-open",
  "components-base-ui--dialog-keyboard",
  "components-base-ui--popover-open",
  "components-base-ui--tooltip-open",
  "components-base-ui--menu-open",
  "components-base-ui--tabs-accordion-table",
  "components-base-ui--surfaces-and-feedback",
  "components-base-ui--portaled-popup-proof",
  "foundations-tokens--active-theme",
];

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
const page = await context.newPage();
const failures = [];

for (const theme of themes) {
  for (const storyId of stories) {
    const url = `${baseUrl}/iframe.html?id=${storyId}&globals=theme:${theme.label}`;
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
