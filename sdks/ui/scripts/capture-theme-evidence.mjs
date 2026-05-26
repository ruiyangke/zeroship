import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) {
  throw new Error("Set STORYBOOK_URL to the running static Storybook URL.");
}

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = process.env.THEME_EVIDENCE_DIR
  ? resolve(process.env.THEME_EVIDENCE_DIR)
  : join(packageRoot, "storybook-static/theme-evidence");
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

const portalUrl = `${baseUrl}/iframe.html?id=components-base-ui--portaled-popup-proof&globals=theme:Dusk`;
await page.goto(portalUrl, { waitUntil: "networkidle" });
await page.locator(".zs-dialog__panel").waitFor({ state: "visible" });
await page.locator(".zs-select__popup").waitFor({ state: "visible" });
await page.screenshot({ path: join(outDir, "dusk-portaled-dialog-select-open.png") });
evidence.push({
  theme: "dusk",
  screenshot: join(outDir, "dusk-portaled-dialog-select-open.png"),
  portalProof: await page.evaluate(() => {
    const dialog = document.querySelector(".zs-dialog__panel");
    const select = document.querySelector(".zs-select__popup");
    const htmlTheme = document.documentElement.dataset.theme;
    const dialogStyles = dialog ? getComputedStyle(dialog) : null;
    const selectStyles = select ? getComputedStyle(select) : null;
    return {
      htmlTheme,
      dialogBackground: dialogStyles?.backgroundColor,
      dialogColor: dialogStyles?.color,
      selectBackground: selectStyles?.backgroundColor,
      selectColor: selectStyles?.color,
    };
  }),
});

await context.close();
await browser.close();
await writeFile(join(outDir, "button-theme-evidence.json"), JSON.stringify(evidence, null, 2));
console.log(JSON.stringify(evidence, null, 2));
