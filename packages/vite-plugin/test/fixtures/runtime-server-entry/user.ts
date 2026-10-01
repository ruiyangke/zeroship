import { env } from "zeroship";
import { state } from "./state";
import greeting from "./hello.greeting";
// Node built-ins by their bare and `node:` names, the plugin's own
// replacements for three of them, and a CommonJS dependency that requires
// built-ins and the kernel module: the runtime answers neither a bare name nor
// `require`.
import path from "path";
import process from "node:process";
import { setTimeout as sleep, setImmediate as immediate, setInterval as every } from "node:timers/promises";
import { createRequire, isBuiltin } from "node:module";
import { Worker } from "node:worker_threads";
import { probe as offThread } from "serializes-off-thread";
import {
  optional,
  joined,
  tsDefaultJoined,
  kernel,
  processEnv,
  slept,
  requiredByCreateRequire,
  constructedWorker,
} from "requires-path";

const code = (probe: () => unknown) => {
  try {
    probe();
    return "accepted";
  } catch (error) {
    return (error as { code?: string }).code;
  }
};

// How a call fails: by throwing, or through the promise it returns.
const failure = async (call: () => Promise<unknown>) => {
  let pending: Promise<unknown>;
  try {
    pending = call();
  } catch (error) {
    return `threw ${(error as { code?: string }).code}`;
  }
  try {
    await pending;
    return "fulfilled";
  } catch (error) {
    return `rejected ${(error as { code?: string }).code}`;
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
      cjsWorker: constructedWorker(),
      esmProcess: process === globalThis.process,
      esmSlept: await sleep(1, "slept"),
      esmCreateRequire: createRequire("/")("node:path").join("k", "l"),
      esmIsBuiltin: isBuiltin("crypto"),
      esmCreateRequireMissing: code(() => createRequire("/")("fs")),
      esmCreateRequireResolve: [createRequire("/").resolve("util"), createRequire("/").resolve("node:buffer")],
      esmCreateRequireResolveMissing: code(() => createRequire("/").resolve("fs")),
      esmTimersSignal: await failure(() => sleep(1, "x", { signal: new AbortController().signal })),
      esmImmediateSignal: await failure(() => immediate("x", { signal: new AbortController().signal })),
      esmIntervalSignal: await failure(() => every(1, "x", { signal: new AbortController().signal }).next()),
      esmWorker: code(() => new Worker("", { eval: true })),
      offThread: await offThread(),
      optional: optional(),
    }),
  },
};
