import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { SOURCE, TARGETS, readSkills } from "../scripts/sync-skills.js";

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const createBin = join(packageRoot, "bin/create.js");

// The agent directories a scaffolded project receives. Claude Code and
// opencode read the same SKILL.md format and differ only in location.
const AGENT_DIRS = [".claude/skills", ".opencode/skills"];

function parseFrontmatter(body) {
  const m = /^---\n([\s\S]*?)\n---\n/.exec(body);
  assert.ok(m, "SKILL.md must open with a YAML frontmatter block");
  const fields = {};
  for (const line of m[1].split("\n")) {
    const kv = /^([a-z]+):\s*(.*)$/.exec(line);
    if (kv) fields[kv[1]] = kv[2];
  }
  return fields;
}

test("there is at least one canonical skill to ship", () => {
  const skills = readSkills();
  assert.ok(skills.length > 0, "skills/ must not be empty");
  for (const s of skills) assert.ok(s.body.length > 0, `${s.name} must not be empty`);
});

test("every skill declares a name matching its directory, and a description", () => {
  for (const { name, body } of readSkills()) {
    const fm = parseFrontmatter(body);
    assert.equal(fm.name, name, `${name}: frontmatter name must match the directory`);
    assert.ok(
      fm.description && fm.description.length > 40,
      `${name}: needs a description saying WHEN to load it (that is all an agent sees)`,
    );
  }
});

test("skills are ASCII only", () => {
  for (const { name, body } of readSkills()) {
    const bad = [...body].find((ch) => ch.codePointAt(0) > 0x7f);
    assert.equal(bad, undefined, `${name}: non-ASCII character ${JSON.stringify(bad)}`);
  }
});

test("the shipped copies match the canonical skills", () => {
  const skills = readSkills();
  for (const target of TARGETS) {
    assert.ok(existsSync(target), `${target} is missing - run scripts/sync-skills.js`);
    const shipped = readdirSync(target).sort();
    assert.deepEqual(
      shipped,
      skills.map((s) => s.name),
      `${target} has a different skill set than ${SOURCE} - run scripts/sync-skills.js`,
    );
    for (const { name, body } of skills) {
      assert.equal(
        readFileSync(join(target, name, "SKILL.md"), "utf-8"),
        body,
        `${target}/${name} drifted from the canonical skill - run scripts/sync-skills.js`,
      );
    }
  }
});

test("a scaffolded project receives the skills for every agent", () => {
  const root = mkdtempSync(join(tmpdir(), "zs-create-skills-"));
  try {
    const result = spawnSync(process.execPath, [createBin, "skills-app"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(result.status, 0, result.stderr);
    const project = join(root, "skills-app");
    const expected = readSkills().map((s) => s.name);

    for (const dir of AGENT_DIRS) {
      for (const name of expected) {
        const file = join(project, dir, name, "SKILL.md");
        assert.ok(existsSync(file), `scaffold is missing ${dir}/${name}/SKILL.md`);
      }
    }
    // Codex reads AGENTS.md rather than skills, so the scaffold must carry it
    // and it must route to every skill by name.
    const agents = readFileSync(join(project, "AGENTS.md"), "utf-8");
    for (const name of expected) {
      assert.match(agents, new RegExp(name), `AGENTS.md must point Codex at ${name}`);
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// A shipped skill that git ignores is present on the author's machine and
// absent from a clean checkout, so it publishes from CI as a half-empty
// scaffold. A repo-wide `.claude/` rule for local editor state matches at any
// depth and does exactly that, which is why this is asserted rather than
// assumed.
test("every shipped skill is visible to git", () => {
  const files = [];
  for (const dir of AGENT_DIRS) {
    for (const { name } of readSkills()) {
      files.push(join("template", dir, name, "SKILL.md"));
    }
  }
  const result = spawnSync("git", ["check-ignore", "--stdin"], {
    cwd: packageRoot,
    input: files.join("\n"),
    encoding: "utf8",
  });
  const ignored = result.stdout.split("\n").filter(Boolean);
  assert.deepEqual(
    ignored,
    [],
    `git ignores these shipped files, so a clean checkout would not have them:\n  ${ignored.join("\n  ")}`,
  );
});
