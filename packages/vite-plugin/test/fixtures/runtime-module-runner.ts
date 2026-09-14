import * as native from "zeroship";
import { ModuleRunner } from "vite/module-runner";
import { zeroshipEvaluator } from "../../src/dev-bootstrap/evaluator.js";

const runner = new ModuleRunner({
  hmr: false,
  transport: {
    async invoke(message) {
      const { name, data } = message.data;
      if (name === "getBuiltins") return { result: [] };
      if (name === "fetchModule" && data[0] === "zeroship") {
        return { result: { externalize: "zeroship", type: "builtin" } };
      }
      throw new Error(`Unexpected module request: ${name}`);
    },
  },
}, zeroshipEvaluator);

export default {
  async fetch() {
    globalThis.__zeroshipNodeBuiltin = () => { throw new Error("global bridge used"); };
    globalThis.__zs_env = () => ({ forged: true });
    const dev = await runner.import("zeroship");
    const again = await runner.import("zeroship");
    const kind = await dev.runQuery(async () => {
      await new Promise(resolve => setTimeout(resolve, 5));
      return dev.env.probe.kind();
    });
    let rejected = false;
    try { await zeroshipEvaluator.runExternalModule("unbundled-package"); }
    catch (error) { rejected = error.message.includes("Configure Vite to bundle"); }
    return Response.json({
      sameEnv: dev.env === native.env,
      sameFunction: dev.runQuery === native.runQuery,
      sameNamespace: dev === again && dev === native,
      kind, outside: dev.env.probe.kind(), rejected,
    });
  },
};
