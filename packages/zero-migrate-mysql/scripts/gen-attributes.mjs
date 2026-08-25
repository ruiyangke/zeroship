// Regenerate this package's typings from the `mysql` crate's exported vocabulary.
// The logic is shared — see scripts/vendor-attribute-codegen.mjs — because three copies
// would be three chances for the vendor packages to disagree about how a declared shape
// becomes a TypeScript type.
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { generate } from "../../../scripts/vendor-attribute-codegen.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const { dialect, count } = await generate({
  vocabularyPath: resolve(here, "../../../crates/zero-migrate-mysql/attribute-vocabulary.json"),
  outPath: resolve(here, "../src/generated/attributes.ts"),
});
console.log(`zero-migrate-${dialect}: ${count} table attribute(s)`);
