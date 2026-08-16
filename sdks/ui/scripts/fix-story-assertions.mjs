#!/usr/bin/env node
/**
 * Point the stories' assertions at data-slot instead of the deleted classes.
 *
 * The stories were deliberately excluded from the class removal, because their
 * own demo markup is plain HTML with its own zeroship-story-* classes. But
 * their play() functions assert on the LIBRARY's classes, which no longer
 * exist, so 55 tests across 29 suites fail. Rendering and asserting are
 * different dependencies and only the first one was considered.
 *
 * Mapping comes from the committed class-to-slot table, never derived: the
 * vocabularies drifted (zs-pricing is slot pricing-table).
 */
import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";

const UI = path.resolve(new URL("..", import.meta.url).pathname);
const TABLE = path.join(UI, "../ui-theme/class-to-slot.tsv");

const map = new Map();
for (const line of fs.readFileSync(TABLE, "utf8").split("\n")) {
  if (!line || line.startsWith("#")) continue;
  const [cls, slot] = line.split("\t");
  if (!cls || !slot || cls.endsWith("--")) continue;
  if (!map.has(cls)) map.set(cls, []);
  if (!map.get(cls).includes(slot)) map.get(cls).push(slot);
}

const ambiguous = new Set();
const unmapped = new Set();

/** The single slot for a class, or null when it cannot be settled here. */
function slotFor(cls) {
  const slots = map.get(cls);
  if (!slots) {
    unmapped.add(cls);
    return null;
  }
  if (slots.length > 1) {
    // Several slots share this class (the eyebrow covers six sections). Which
    // one a given story means is a judgement call, so leave it and report.
    ambiguous.add(`${cls} -> ${slots.join(", ")}`);
    return null;
  }
  return slots[0];
}

const files = execFileSync("git", ["ls-files", "src/stories"], {
  cwd: UI,
  encoding: "utf8",
})
  .trim()
  .split("\n")
  .filter((f) => f.endsWith(".tsx"));

let changed = 0;
let edits = 0;
for (const rel of files) {
  const file = path.join(UI, rel);
  const src = fs.readFileSync(file, "utf8");
  let out = src;

  // toHaveClass("zs-x")  ->  toHaveAttribute("data-slot", "x")
  out = out.replace(/toHaveClass\("(zs-[a-z0-9_-]+)"\)/g, (whole, cls) => {
    const slot = slotFor(cls);
    if (!slot) return whole;
    edits++;
    return `toHaveAttribute("data-slot", "${slot}")`;
  });

  // querySelector(".zs-x") / querySelectorAll(".zs-x")
  out = out.replace(
    /(querySelector(?:All)?)\("\.(zs-[a-z0-9_-]+)"\)/g,
    (whole, fn, cls) => {
      const slot = slotFor(cls);
      if (!slot) return whole;
      edits++;
      return `${fn}('[data-slot="${slot}"]')`;
    },
  );

  if (out !== src) {
    fs.writeFileSync(file, out);
    changed++;
  }
}

console.log(`files: ${changed}  assertions rewritten: ${edits}`);
if (ambiguous.size > 0) {
  console.error("AMBIGUOUS, left alone (one class, several slots):\n  " + [...ambiguous].join("\n  "));
}
if (unmapped.size > 0) {
  console.error("NOT IN THE TABLE, left alone:\n  " + [...unmapped].join("\n  "));
}
