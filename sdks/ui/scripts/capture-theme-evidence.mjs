import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { chromium } from "@playwright/test";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) {
  throw new Error("Set STORYBOOK_URL to the running static Storybook URL.");
}

const outDir = process.env.THEME_EVIDENCE_DIR ?? "storybook-static/theme-evidence";
const themes = [
  { label: "Atelier", value: "atelier" },
  { label: "Studio", value: "studio" },
  { label: "Dusk", value: "dusk" },
];
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 960, height: 540 } });
const page = await context.newPage();
const evidence = [];

await mkdir(outDir, { recursive: true });

for (const theme of themes) {
  const url = `${baseUrl}/iframe.html?id=primitives-button--variants&globals=theme:${theme.label}`;
  await page.goto(url, { waitUntil: "networkidle" });
  const button = page.locator(".zs-button--primary").first();
  await button.screenshot({ path: join(outDir, `button-${theme.value}.png`) });
  evidence.push({
    theme: theme.value,
    screenshot: join(outDir, `button-${theme.value}.png`),
    computed: await button.evaluate((el) => {
      const styles = getComputedStyle(el);
      return {
        backgroundColor: styles.backgroundColor,
        color: styles.color,
        borderRadius: styles.borderRadius,
        fontFamily: styles.fontFamily,
      };
    }),
  });
}

await context.close();
await browser.close();
await writeFile(join(outDir, "button-theme-evidence.json"), JSON.stringify(evidence, null, 2));
console.log(JSON.stringify(evidence, null, 2));
