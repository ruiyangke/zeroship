#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "../../..");
const shortPrefix = ["z", "s"].join("");

const textExtensions = new Set([
  ".cjs",
  ".css",
  ".html",
  ".js",
  ".jsx",
  ".json",
  ".md",
  ".mjs",
  ".ts",
  ".tsx",
  ".yaml",
  ".yml",
]);

const trackedFiles = execFileSync(
  "git",
  ["ls-files", "-z", "--", "sdks", "examples"],
  { cwd: repoRoot },
)
  .toString("utf8")
  .split("\0")
  .filter((file) => file && textExtensions.has(path.extname(file)));

const original = new Map();
const pending = new Map();

for (const file of trackedFiles) {
  const buffer = readFileSync(path.join(repoRoot, file));
  const source = buffer.toString("utf8");
  if (!Buffer.from(source, "utf8").equals(buffer)) {
    throw new Error(`refusing to rewrite non-UTF-8 file: ${file}`);
  }
  original.set(file, source);
  pending.set(file, source);
}

function occurrenceCount(source, value) {
  return source.split(value).length - 1;
}

function replaceEverywhere(label, from, to, counts) {
  let count = 0;
  for (const [file, source] of pending) {
    const fileCount = occurrenceCount(source, from);
    if (fileCount === 0) continue;
    pending.set(file, source.replaceAll(from, to));
    count += fileCount;
  }
  counts.set(label, count);
}

function replaceInFile(label, file, from, to, counts) {
  const source = pending.get(file);
  if (source === undefined) throw new Error(`tracked file not found: ${file}`);
  const count = occurrenceCount(source, from);
  if (count > 0) pending.set(file, source.replaceAll(from, to));
  counts.set(label, (counts.get(label) ?? 0) + count);
}

function replaceOutsideBlockComments(source, from, to) {
  let cursor = 0;
  let count = 0;
  let output = "";

  for (const match of source.matchAll(/\/\*[\s\S]*?\*\//g)) {
    const beforeComment = source.slice(cursor, match.index);
    count += occurrenceCount(beforeComment, from);
    output += beforeComment.replaceAll(from, to) + match[0];
    cursor = match.index + match[0].length;
  }

  const tail = source.slice(cursor);
  count += occurrenceCount(tail, from);
  return { source: output + tail.replaceAll(from, to), count };
}

function protectedThemeClassReferences(files) {
  const references = [];
  const classPattern = new RegExp(
    `(?<!-)\\.${shortPrefix}-[a-z0-9_-]+`,
    "g",
  );

  for (const [file, source] of files) {
    if (!file.startsWith("sdks/ui-theme/")) continue;
    for (const comment of source.matchAll(/\/\*[\s\S]*?\*\//g)) {
      for (const className of comment[0].matchAll(classPattern)) {
        references.push(`${file}\t${className[0]}`);
      }
    }
  }

  return references.sort();
}

const protectedBefore = protectedThemeClassReferences(pending);
const counts = new Map();

const translator = "sdks/ui-theme/scripts/translate-selectors.mjs";
const oldStoryPrefix = `${shortPrefix}-story-`;
const oldGuard =
  `  if (classes.some((c) => c.startsWith("${oldStoryPrefix}"))) return unit;\n`;
const newGuard =
  '  if (unit.includes(".zeroship-story-")) return unit;\n';
let translatorSource = pending.get(translator);
if (translatorSource.includes(oldGuard)) {
  translatorSource = translatorSource.replace(oldGuard, "");
  if (!translatorSource.includes(newGuard)) {
    translatorSource = translatorSource.replace(
      "function translateCompound(unit) {\n",
      `function translateCompound(unit) {\n${newGuard}`,
    );
  }
  pending.set(translator, translatorSource);
  counts.set("translator story guard", 1);
} else {
  counts.set("translator story guard", 0);
}

const oldCustomPropertyPrefix = `--${shortPrefix}-`;
replaceEverywhere(
  "custom property prefixes",
  oldCustomPropertyPrefix,
  "--zeroship-",
  counts,
);
replaceEverywhere(
  "storybook class prefixes",
  oldStoryPrefix,
  "zeroship-story-",
  counts,
);

for (const [label, from, to] of [
  ["storybook demo prefixes", `${shortPrefix}-demo-`, "zeroship-demo-"],
  [
    "as-child fixture classes",
    `${shortPrefix}-aschild-`,
    "zeroship-aschild-",
  ],
  [
    "collapsible fixture classes",
    `${shortPrefix}-collapsible-aschild-`,
    "zeroship-collapsible-aschild-",
  ],
  [
    "drawer fixture classes",
    `${shortPrefix}-drawer-close-aschild-extra`,
    "zeroship-drawer-close-aschild-extra",
  ],
  ["child fixture classes", `${shortPrefix}-child-cls`, "zeroship-child-cls"],
  [
    "wrapper fixture classes",
    `${shortPrefix}-wrapper-cls`,
    "zeroship-wrapper-cls",
  ],
  ["select fixture classes", `${shortPrefix}-select-basic`, "zeroship-select-basic"],
  ["alert button sentinels", `__${shortPrefix}AlertButton`, "__zeroshipAlertButton"],
  [
    "toast action counters",
    `__${shortPrefix}ToastActionCalls`,
    "__zeroshipToastActionCalls",
  ],
  ["button keyframes", `${shortPrefix}-button-spin`, "zeroship-button-spin"],
  [
    "progress keyframes",
    `${shortPrefix}-progress-shimmer`,
    "zeroship-progress-shimmer",
  ],
  [
    "skeleton keyframes",
    `${shortPrefix}-skeleton-shimmer`,
    "zeroship-skeleton-shimmer",
  ],
  ["spinner keyframes", `${shortPrefix}-spinner-spin`, "zeroship-spinner-spin"],
]) {
  replaceEverywhere(label, from, to, counts);
}

replaceInFile(
  "preview card ids",
  "sdks/ui/src/components/PreviewCard/PreviewCard.tsx",
  `${shortPrefix}-previewcard-`,
  "zeroship-previewcard-",
  counts,
);
replaceInFile(
  "tooltip ids",
  "sdks/ui/src/components/Tooltip/Tooltip.tsx",
  `${shortPrefix}-tooltip-`,
  "zeroship-tooltip-",
  counts,
);
replaceInFile(
  "toast ids",
  "sdks/ui/src/components/Toast/useToast.ts",
  `${shortPrefix}-toast-`,
  "zeroship-toast-",
  counts,
);
replaceInFile(
  "slider story ids",
  "sdks/ui/src/stories/Slider.stories.tsx",
  `${shortPrefix}-slider-range-label`,
  "zeroship-slider-range-label",
  counts,
);

const oldFilterClass = `${shortPrefix}-filter-bar__search`;
replaceInFile(
  "filter bar wrapper classes",
  "sdks/ui/src/blocks/FilterBar/FilterBar.tsx",
  oldFilterClass,
  "zeroship-filter-bar__search",
  counts,
);

const filterCss = "sdks/ui-theme/src/blocks/FilterBar/FilterBar.css";
const filterResult = replaceOutsideBlockComments(
  pending.get(filterCss),
  oldFilterClass,
  "zeroship-filter-bar__search",
);
pending.set(filterCss, filterResult.source);
counts.set("filter bar selectors", filterResult.count);

for (const [label, oldValue] of [
  ["custom property prefix", oldCustomPropertyPrefix],
  ["storybook class prefix", oldStoryPrefix],
]) {
  const leftovers = [...pending]
    .filter(([, source]) => source.includes(oldValue))
    .map(([file]) => file);
  if (leftovers.length > 0) {
    throw new Error(`${label} remains in: ${leftovers.join(", ")}`);
  }
}

const protectedAfter = protectedThemeClassReferences(pending);
if (JSON.stringify(protectedAfter) !== JSON.stringify(protectedBefore)) {
  throw new Error("protected ui-theme class comments changed");
}

let changedFiles = 0;
for (const [file, source] of pending) {
  if (source === original.get(file)) continue;
  writeFileSync(path.join(repoRoot, file), source);
  changedFiles += 1;
}

for (const [label, count] of counts) {
  console.log(`${label}: ${count}`);
}
console.log(`changed files: ${changedFiles}`);
