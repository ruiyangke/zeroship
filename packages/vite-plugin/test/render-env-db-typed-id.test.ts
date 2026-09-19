import { test } from "node:test";
import assert from "node:assert/strict";
import {
  renderGeneratedEnvDb,
  type RuntimeDescriptor,
} from "../src/gen-types/render-env-db.js";

test("generated database types round-trip a typed id as t.typedId(prefix)", () => {
  const descriptor: RuntimeDescriptor = {
    collections: {
      posts: {
        fields: {
          id: { type: "string", maxLength: 36, required: true, idPrefix: "post" },
          // Same storage, no declared prefix: must NOT become a typed id.
          slug: { type: "string", maxLength: 36, required: true },
        },
      },
    },
  };
  const output = renderGeneratedEnvDb(descriptor);
  assert.match(
    output,
    /id: t\.typedId\("post"\)\.required\(\)/,
    "a declared prefix must survive as t.typedId, not degrade to a bounded string",
  );
  assert.doesNotMatch(
    output,
    /id: t\.string\(\{ length: 36 \}\)/,
    "the typed id must not degrade to a plain bounded string",
  );
  assert.match(output, /slug: t\.string\(\)\.required\(\)/);
});
