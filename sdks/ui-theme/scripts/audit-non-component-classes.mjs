#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { readdir, readFile } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const themeRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = resolve(themeRoot, "../..");
const uiRoot = join(repoRoot, "sdks/ui");

const slotRoots = [
  join(uiRoot, "src/components"),
  join(uiRoot, "src/layouts"),
  join(uiRoot, "src/blocks"),
];

const referenceRoots = [
  {
    category: "component",
    root: join(uiRoot, "src"),
    exclude: join(uiRoot, "src/stories"),
  },
  { category: "storybook", root: join(uiRoot, "src/stories") },
  { category: "storybook", root: join(uiRoot, ".storybook") },
  {
    category: "app",
    root: join(repoRoot, "examples/issue-tracker/src"),
  },
];

async function filesUnder(root) {
  const entries = await readdir(root, { withFileTypes: true });
  const files = [];

  for (const entry of entries) {
    const path = join(root, entry.name);
    if (entry.isDirectory()) files.push(...(await filesUnder(path)));
    else if (entry.isFile()) files.push(path);
  }

  return files;
}

async function cssClassNames() {
  const names = new Set();
  const cssFiles = (await filesUnder(join(themeRoot, "src"))).filter((path) =>
    path.endsWith(".css"),
  );

  for (const path of cssFiles) {
    const css = await readFile(path, "utf8");
    for (const match of css.matchAll(/\.zs-[a-z0-9_-]+/g)) {
      names.add(match[0].slice(1));
    }
  }

  return [...names].sort();
}

async function validSlots() {
  const slots = new Set();

  for (const root of slotRoots) {
    const sourceFiles = (await filesUnder(root)).filter((path) =>
      path.endsWith(".tsx"),
    );
    for (const path of sourceFiles) {
      const source = await readFile(path, "utf8");
      for (const match of source.matchAll(/data-slot="([^"]*)"/g)) {
        slots.add(match[1]);
      }
    }
  }

  return slots;
}

function modifierBlocks() {
  const output = execFileSync(
    process.execPath,
    [join(uiRoot, "scripts/slot-attr-map.mjs")],
    { cwd: uiRoot, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] },
  );
  return new Set(Object.keys(JSON.parse(output)));
}

function derivedSlot(className) {
  return className
    .slice("zs-".length)
    .replaceAll("__", "-")
    .replaceAll("--", "-");
}

function bemBlock(className) {
  return className.slice("zs-".length).split(/__|--/, 1)[0];
}

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function literalReferenceCount(source, className) {
  const pattern = new RegExp(
    `(?<![A-Za-z0-9_-])${escapeRegex(className)}(?![A-Za-z0-9_-])`,
    "g",
  );
  return [...source.matchAll(pattern)].length;
}

function dynamicReferenceCount(source, className) {
  let count = 0;

  for (const match of source.matchAll(/`(zs-[a-z0-9_-]*(?:\$\{[^}]+\}[a-z0-9_-]*)+)`/g)) {
    const dynamicPattern = match[1]
      .split(/(\$\{[^}]+\})/g)
      .map((part) =>
        part.startsWith("${") ? "[a-z0-9_-]+" : escapeRegex(part),
      )
      .join("");
    if (new RegExp(`^${dynamicPattern}$`).test(className)) count += 1;
  }

  return count;
}

async function referencesFor(classNames) {
  const references = new Map(
    classNames.map((className) => [
      className,
      { component: 0, storybook: 0, app: 0, dynamic: 0 },
    ]),
  );

  for (const { category, root, exclude } of referenceRoots) {
    for (const path of await filesUnder(root)) {
      if (exclude && (path === exclude || path.startsWith(`${exclude}/`))) {
        continue;
      }

      const source = await readFile(path, "utf8");
      if (source.includes("\0")) continue;

      for (const className of classNames) {
        const literal = literalReferenceCount(source, className);
        const dynamic = dynamicReferenceCount(source, className);
        references.get(className)[category] += literal + dynamic;
        references.get(className).dynamic += dynamic;
      }
    }
  }

  return references;
}

const allClasses = await cssClassNames();
const slots = await validSlots();
const blocks = modifierBlocks();
const nonComponentClasses = allClasses.filter(
  (className) =>
    !slots.has(derivedSlot(className)) && !blocks.has(bemBlock(className)),
);
const references = await referencesFor(nonComponentClasses);

const rows = nonComponentClasses.map((className) => {
  const counts = references.get(className);
  const total = counts.component + counts.storybook + counts.app;
  let disposition;
  let reason;

  if (counts.component > 0 || counts.app > 0) {
    disposition = "keep";
    reason = counts.app > 0 ? "app or UI source" : "UI source";
  } else if (counts.storybook > 0) {
    disposition = "move";
    reason = "Storybook only";
  } else {
    disposition = "delete";
    reason = "zero references";
  }

  return { className, counts, total, disposition, reason };
});

const dispositionCounts = { delete: 0, move: 0, keep: 0 };
for (const row of rows) dispositionCounts[row.disposition] += 1;

console.log(`Theme class selectors: ${allClasses.length}`);
console.log(`Valid slots: ${slots.size}`);
console.log(`Modifier-map blocks: ${blocks.size}`);
console.log(`Component classes: ${allClasses.length - nonComponentClasses.length}`);
console.log(`Non-component classes: ${nonComponentClasses.length}`);
console.log(
  `Dispositions: delete=${dispositionCounts.delete} move=${dispositionCounts.move} keep=${dispositionCounts.keep}`,
);

if (!process.argv.includes("--count-only")) {
  console.log("");
  console.log(
    "| Class | Component refs | Storybook refs | App refs | Total | Disposition | Reason |",
  );
  console.log(
    "| --- | ---: | ---: | ---: | ---: | --- | --- |",
  );
  for (const { className, counts, total, disposition, reason } of rows) {
    const dynamic = counts.dynamic > 0 ? ` (${counts.dynamic} dynamic)` : "";
    console.log(
      `| .${className} | ${counts.component} | ${counts.storybook} | ${counts.app} | ${total}${dynamic} | ${disposition} | ${reason} |`,
    );
  }
}

if (process.argv.includes("--paths")) {
  console.log("");
  console.log(`Theme root: ${relative(repoRoot, themeRoot)}`);
  for (const { category, root } of referenceRoots) {
    console.log(`${category}: ${relative(repoRoot, root)}`);
  }
}

if (process.argv.includes("--require-zero") && nonComponentClasses.length > 0) {
  console.error("Expected zero non-component classes.");
  process.exitCode = 1;
}
