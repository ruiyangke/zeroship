#!/usr/bin/env node
/**
 * Rewrite class names inside CSS COMMENTS to the attribute form the file
 * actually uses.
 *
 * The selectors were translated in 4058a131a and the components stopped
 * emitting classes in 574330b6b, but the comments were deliberately preserved
 * verbatim by that transform -- rewriting docs mid-substitution would have
 * corrupted them while looking correct. This pays that debt separately, so
 * each diff stays reviewable on its own.
 *
 * Comments only. If this script ever changes a selector, it is broken: the
 * real class-selector count is 0 and must stay 0.
 */
import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";

const ROOT = path.resolve(new URL("../../..", import.meta.url).pathname);
const THEME = path.resolve(new URL("..", import.meta.url).pathname);

/** class -> [slot, ...] from the committed, non-regenerable table. */
const classMap = new Map();
for (const line of fs
  .readFileSync(path.join(THEME, "class-to-slot.tsv"), "utf8")
  .split("\n")) {
  if (!line || line.startsWith("#")) continue;
  const [cls, slot] = line.split("\t");
  if (!cls || !slot || cls.endsWith("--")) continue;
  if (!classMap.has(cls)) classMap.set(cls, []);
  if (!classMap.get(cls).includes(slot)) classMap.get(cls).push(slot);
}

const attrMap = JSON.parse(
  execFileSync("node", ["scripts/slot-attr-map.mjs"], {
    cwd: path.join(ROOT, "sdks/ui"),
    encoding: "utf8",
    maxBuffer: 32 * 1024 * 1024,
    stdio: ["ignore", "pipe", "ignore"],
  }),
);

/** Modifiers carried by an attribute that is not in the value map. */
const EXPLICIT = {
  vertical: ["orientation", "vertical"],
  horizontal: ["orientation", "horizontal"],
  destructive: ["intent", "destructive"],
  sticky: ["sticky", null],
  truncate: ["truncate", null],
  removable: ["removable", null],
  filter: ["filter", null],
  ellipsis: ["ellipsis", null],
  divided: ["divided", null],
  inline: ["inline", null],
  explicit: ["explicit", null],
  intrinsic: ["intrinsic", null],
  actions: ["actions", null],
};

const unresolved = new Set();

function render(cls) {
  // FilterBar's is still a real class: the parent styles a child Input
  // instance, which is composition rather than a part it declares.
  if (cls === "zs-filter-bar__search") return ".zeroship-filter-bar__search";

  // Modifier first. Collapsing `.zs-icon--md` to [data-slot="icon"] drops the
  // size and documents something false -- the partial run did exactly that
  // once, which is why this path is checked before the direct lookup.
  const m = cls.match(/^(.+?)--([a-z0-9-]+)$/);
  if (m) {
    const [, base, value] = m;
    const slots = classMap.get(base);
    if (slots) {
      const block = base.slice("zs-".length).replace(/__/g, "-");
      const pair = attrMap[block]?.[value]
        ? [attrMap[block][value], value]
        : EXPLICIT[value];
      if (pair) {
        const [attr, v] = pair;
        const sel = slots.map((s) => `[data-slot="${s}"]`).join(", ");
        const tail = v === null ? `[data-${attr}]` : `[data-${attr}="${v}"]`;
        return slots.length === 1 ? sel + tail : `:is(${sel})${tail}`;
      }
    }
  }

  const slots = classMap.get(cls);
  if (!slots) {
    unresolved.add(cls);
    return null;
  }
  return slots.length === 1
    ? `[data-slot="${slots[0]}"]`
    : slots.map((s) => `[data-slot="${s}"]`).join(", ");
}

const files = execFileSync("git", ["ls-files", "sdks/ui-theme"], {
  cwd: ROOT,
  encoding: "utf8",
})
  .trim()
  .split("\n")
  .filter((f) => f.endsWith(".css"));

let changed = 0;
let replaced = 0;
for (const rel of files) {
  const file = path.join(ROOT, rel);
  const src = fs.readFileSync(file, "utf8");
  const out = src.replace(/\/\*[\s\S]*?\*\//g, (comment) =>
    comment.replace(/\.(zs-[a-z0-9_-]+)/g, (whole, cls) => {
      const r = render(cls);
      if (r === null) return whole;
      replaced++;
      return r;
    }),
  );
  if (out !== src) {
    fs.writeFileSync(file, out);
    changed++;
  }
}
console.log(`files: ${changed}  references rewritten: ${replaced}`);
if (unresolved.size > 0) {
  console.error("left as-is (not in the table):\n  " + [...unresolved].join("\n  "));
}
