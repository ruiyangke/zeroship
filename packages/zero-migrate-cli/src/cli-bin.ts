#!/usr/bin/env node
// The `zero-migrate` executable entry. The leading shebang is
// preserved by esbuild onto the emitted `dist/cli-bin.js`, and package.json maps the
// `zero-migrate` bin to it. This file only wires argv → `main` → process exit; all
// logic lives in `cli.ts`.

import { main } from "./cli.js";

main(process.argv.slice(2))
  .then((code) => {
    process.exitCode = code;
  })
  .catch((e) => {
    process.stderr.write(`zero-migrate: ${(e as Error).stack ?? String(e)}\n`);
    process.exitCode = 1;
  });
