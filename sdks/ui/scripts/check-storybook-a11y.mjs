import { chromium } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) {
  throw new Error("Set STORYBOOK_URL to the running static Storybook URL.");
}

const themes = [
  { label: "Atelier", value: "atelier" },
  { label: "Studio", value: "studio" },
  { label: "Dusk", value: "dusk" },
];
const stories = [
  "primitives-button--variants",
  "primitives-forms--input-textarea-select",
  "primitives-surfaces--cards-badges-chips-empty-state-spinner",
  "primitives-overlays-and-data--dialog-tabs-toast-table",
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
    if (results.violations.length > 0) {
      failures.push({ theme: theme.value, storyId, violations: results.violations });
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

console.log(`A11y clean for ${stories.length} stories across ${themes.length} themes.`);
