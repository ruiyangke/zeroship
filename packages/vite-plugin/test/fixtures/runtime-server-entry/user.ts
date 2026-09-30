import { env } from "zeroship";
import { state } from "./state";
import greeting from "./hello.greeting";
// Node built-ins by their bare and `node:` names, the plugin's own
// replacements for three of them, and a CommonJS dependency that requires
// built-ins and the kernel module: the runtime answers neither a bare name nor
// `require`.
import path from "path";
import process from "node:process";
import { setTimeout as sleep } from "node:timers/promises";
import { createRequire, isBuiltin } from "node:module";
import {
  optional,
  joined,
  tsDefaultJoined,
  kernel,
  processEnv,
  slept,
  requiredByCreateRequire,
} from "requires-path";

const code = (probe: () => unknown) => {
  try {
    probe();
    return "accepted";
  } catch (error) {
    return (error as { code?: string }).code;
  }
};

export default {
  label: "original receiver",
  fetch(request: Request) {
    return Response.json({ label: this.label, path: new URL(request.url).pathname, kind: env.probe.kind() });
  },
  rpc: {
    inspect: () => ({ ...state }),
    greeting: () => greeting,
    builtins: async () => ({
      imported: path.join("a", "b"),
      required: joined(),
      tsDefault: tsDefaultJoined(),
      kernel: kernel(),
      cjsProcess: processEnv(),
      cjsSlept: await slept(),
      cjsCreateRequire: requiredByCreateRequire(),
      esmProcess: process === globalThis.process,
      esmSlept: await sleep(1, "slept"),
      esmCreateRequire: createRequire("/")("node:path").join("k", "l"),
      esmIsBuiltin: isBuiltin("crypto"),
      esmCreateRequireMissing: code(() => createRequire("/")("fs")),
      esmTimersSignal: code(() => sleep(1, "x", { signal: new AbortController().signal })),
      optional: optional(),
    }),
  },
};
