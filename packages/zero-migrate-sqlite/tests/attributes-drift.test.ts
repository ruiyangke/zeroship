// The committed typings must equal what the generator produces from the Rust artifact.
//
// Without this, adding a knob in Rust and forgetting to regenerate ships a package whose
// types silently lack it: the author gets a type error on a knob the engine accepts,
// and nothing anywhere says the file is stale. The same failure in reverse — hand-editing
// the generated file — is caught too, since the next regeneration reverts it.
import { strict as assert } from "node:assert";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const generatedPath = resolve(here, "../src/generated/attributes.ts");
const scriptPath = resolve(here, "../scripts/gen-attributes.mjs");
const vocabularyPath = resolve(
  here,
  "../../../crates/zero-migrate-sqlite/attribute-vocabulary.json",
);

test("the committed typings match the Rust-exported vocabulary", () => {
  const before = readFileSync(generatedPath, "utf8");
  execFileSync(process.execPath, [scriptPath], { stdio: "pipe" });
  const after = readFileSync(generatedPath, "utf8");
  assert.equal(
    after,
    before,
    "src/generated/attributes.ts is stale. Regenerate with:\n" +
      "  UPDATE_VOCABULARY=1 cargo test -p zero-migrate-sqlite --test attribute_vocabulary_export\n" +
      "  node packages/zero-migrate-sqlite/scripts/gen-attributes.mjs\n" +
      "and commit both.",
  );
});

test("every declared attribute reaches the typings", () => {
  const doc = JSON.parse(readFileSync(vocabularyPath, "utf8"));
  const generated = readFileSync(generatedPath, "utf8");

  // The anti-fail-open floor: an empty artifact would make the loop below vacuous, and
  // the whole file would pass while asserting nothing about anything.
  assert.ok(
    doc.attributes.length >= 3,
    `the vocabulary carries ${doc.attributes.length} attribute(s); a near-empty artifact ` +
      "makes this test vacuous",
  );

  for (const def of doc.attributes) {
    const leaf = def.key.slice(doc.dialect.length + 1);
    assert.ok(
      generated.includes(`${leaf}?:`),
      `${def.key} is declared in Rust but absent from the generated typings`,
    );
    assert.ok(
      generated.includes(def.docs.split(/\s+/).slice(0, 4).join(" ")),
      `${def.key}'s documentation did not survive generation — the doc comment is what ` +
        "the author reads on hover, so losing it is a real regression",
    );
  }

  // The namespace key must be the dialect id, not a hand-picked alias. This is the
  // property that makes `npm install zero-migrate-mysql` yield `mysql:` for free.
  assert.ok(
    generated.includes(`${doc.dialect}?:`),
    `the namespace key must be the dialect id "${doc.dialect}"`,
  );
});
