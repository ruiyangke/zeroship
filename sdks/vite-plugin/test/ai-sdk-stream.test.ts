/**
 * Phase 4 — synthetic-entry SSE encoding.
 *
 * The synthetic SSR entry's `_zsFetch` slow path catches handlers that
 * return an async iterator and frames them per the Vercel AI-SDK Data
 * Stream Protocol:
 *
 *   `0:"text"\n`     string yields
 *   `2:[<json>]\n`   object yields
 *   `e:{...}\n`      mid-stream error envelope (zeroship extension)
 *   `d:{}\n`         done
 *
 * Both the kernel fast-path (handled in the runtime crate) and the
 * synthetic entry's `default.fetch` slow path emit the SAME wire shape.
 *
 * These tests instantiate the synthetic entry source as a real ESM
 * module via dynamic import. Each test stubs the user module and the
 * registry virtual module, then invokes `_zsFetch` with a synthetic
 * Request and reads the response stream as text.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { writeFile, mkdtemp, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";

import {
  buildServerEntrySource,
  RPC_REGISTRY_SOURCE,
} from "../src/rpc-registry.js";

interface UserModuleConfig {
  /** Map of exported function names to their implementations. */
  exports: Record<string, (...args: unknown[]) => unknown>;
  /** Optional config blocks attached as `<fn>.config`. */
  configs?: Record<string, Record<string, unknown>>;
  /** Optional default export — fetch handler. */
  defaultFetch?: (req: Request) => Promise<Response> | Response;
}

/**
 * Materialize the synthetic entry plus a stub user module + registry
 * module, then return the dynamically-imported entry's namespace.
 *
 * The stub user module installs the user functions at module load time
 * and registers them with the registry via `_zsRegister`. After import,
 * the synthetic entry's `default.fetch` is callable directly.
 */
async function loadSyntheticEntry(user: UserModuleConfig): Promise<{
  fetch: (req: Request) => Promise<Response>;
  cleanup: () => Promise<void>;
}> {
  const dir = await mkdtemp(join(tmpdir(), "zsrpc-aisdk-"));

  // Step 1: write the registry virtual module verbatim.
  const registryPath = join(dir, "registry.mjs");
  await writeFile(registryPath, RPC_REGISTRY_SOURCE, "utf8");

  // Step 2: write the user module — registers fns with the registry on
  // load. We re-export `default` only when the test asks for one.
  const fnNames = Object.keys(user.exports);
  // Stash the impls + configs on globalThis so the user module can
  // reach them without us serializing closures into source.
  const userKey = `__zs_user_${dir.replace(/[^a-zA-Z0-9]/g, "_")}`;
  (globalThis as unknown as Record<string, unknown>)[userKey] = {
    exports: user.exports,
    configs: user.configs ?? {},
    defaultFetch: user.defaultFetch,
  };

  const userExportsSrc = fnNames
    .map((n) => {
      const cfgKey = user.configs?.[n] ? `, config: __zsTest.configs[${JSON.stringify(n)}]` : "";
      return `export const ${n} = Object.assign(
        function (...args) { return __zsTest.exports[${JSON.stringify(n)}].apply(null, args); },
        { /* placeholder */ }${cfgKey ? "" : ""}
      );
      ${user.configs?.[n] ? `${n}.config = __zsTest.configs[${JSON.stringify(n)}];` : ""}`;
    })
    .join("\n");
  const defaultExportSrc = user.defaultFetch
    ? `export default { fetch: __zsTest.defaultFetch };`
    : "";
  const registrationsSrc = fnNames
    .map((n) => `_zsRegister(${JSON.stringify(n)}, ${n});`)
    .join("\n");

  const userPath = join(dir, "user.mjs");
  const userSrc = `import { _zsRegister } from ${JSON.stringify(pathToFileURL(registryPath).href)};
const __zsTest = globalThis[${JSON.stringify(userKey)}];
${userExportsSrc}
${defaultExportSrc}
${registrationsSrc}
`;
  await writeFile(userPath, userSrc, "utf8");

  // Step 3: write the synthetic entry — its source uses a direct
  // import for the user module + registry. Because `buildServerEntrySource`
  // hard-codes `virtual:zeroship/_rpc-registry`, we need to inline a
  // version that points at our temp registry path.
  let entrySrc = buildServerEntrySource({
    userEntryRel: pathToFileURL(userPath).href,
  });
  // Re-route the registry import to our temp file. The buildServerEntrySource
  // emits `import { dispatch as _zsDispatch } from "virtual:zeroship/_rpc-registry";`.
  entrySrc = entrySrc.replace(
    "virtual:zeroship/_rpc-registry",
    pathToFileURL(registryPath).href,
  );
  const entryPath = join(dir, "entry.mjs");
  await writeFile(entryPath, entrySrc, "utf8");

  const mod = (await import(pathToFileURL(entryPath).href)) as {
    default: { fetch: (req: Request) => Promise<Response> };
  };
  return {
    fetch: mod.default.fetch,
    cleanup: async () => {
      delete (globalThis as unknown as Record<string, unknown>)[userKey];
      await rm(dir, { recursive: true, force: true });
    },
  };
}

async function readBody(res: Response): Promise<string> {
  const reader = res.body!.getReader();
  const dec = new TextDecoder();
  let out = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    out += dec.decode(value, { stream: true });
  }
  out += dec.decode();
  return out;
}

describe("synthetic-entry — AI-SDK Data Stream wire", () => {
  test("string-yielding generator emits 0:<json> per yield + d:{}", async () => {
    const { fetch, cleanup } = await loadSyntheticEntry({
      exports: {
        async sayHello() {
          // Async generator returning strings.
          return (async function* () {
            yield "Hi";
            yield " there";
          })();
        },
      },
    });
    try {
      const res = await fetch(
        new Request("http://localhost/_zs/v1/sayHello", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: "{}",
        }),
      );
      assert.equal(res.status, 200);
      assert.equal(res.headers.get("content-type"), "text/event-stream");
      const text = await readBody(res);
      assert.match(text, /^0:"Hi"\n/m, `expected 0:"Hi" line, got: ${text}`);
      assert.match(text, /^0:" there"\n/m, `expected 0:" there" line, got: ${text}`);
      assert.match(text, /d:\{\}\n/, `expected d:{} done, got: ${text}`);
      // Legacy SSE shape must not appear.
      assert.doesNotMatch(text, /event: yield/, `legacy SSE leaked: ${text}`);
    } finally {
      await cleanup();
    }
  });

  test("object-yielding generator emits 2:[<json>] per yield + d:{}", async () => {
    const { fetch, cleanup } = await loadSyntheticEntry({
      exports: {
        async streamTodos() {
          return (async function* () {
            yield { id: 1, text: "first" };
            yield { id: 2, text: "second" };
          })();
        },
      },
    });
    try {
      const res = await fetch(
        new Request("http://localhost/_zs/v1/streamTodos", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: "{}",
        }),
      );
      const text = await readBody(res);
      assert.match(
        text,
        /2:\[\{"id":1,"text":"first"\}\]\n/,
        `got: ${text}`,
      );
      assert.match(
        text,
        /2:\[\{"id":2,"text":"second"\}\]\n/,
        `got: ${text}`,
      );
      assert.match(text, /d:\{\}\n/, `got: ${text}`);
    } finally {
      await cleanup();
    }
  });

  test("mid-stream throw emits e:<envelope> then d:{}", async () => {
    const { fetch, cleanup } = await loadSyntheticEntry({
      exports: {
        async boom() {
          return (async function* () {
            yield { id: 1 };
            const err = new Error("upstream gone") as Error & {
              code?: string;
              details?: unknown;
              retryable?: boolean;
            };
            err.code = "UNAVAILABLE";
            err.details = { hint: "demo" };
            err.retryable = true;
            throw err;
          })();
        },
      },
    });
    try {
      const res = await fetch(
        new Request("http://localhost/_zs/v1/boom", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: "{}",
        }),
      );
      const text = await readBody(res);
      // First yield went out OK.
      assert.match(text, /2:\[\{"id":1\}\]\n/, `got: ${text}`);
      // Then the structured error envelope.
      const eMatch = text.match(/e:(\{.*?\})\n/);
      assert.ok(eMatch, `e: envelope missing in ${text}`);
      const env = JSON.parse(eMatch![1]);
      assert.equal(env.message, "upstream gone");
      assert.equal(env.code, "UNAVAILABLE");
      assert.deepEqual(env.details, { hint: "demo" });
      assert.equal(env.retryable, true);
      // Final d:{} after the error.
      const eIdx = text.indexOf("e:");
      assert.match(text.slice(eIdx), /d:\{\}\n/, `expected d:{} after error: ${text}`);
    } finally {
      await cleanup();
    }
  });

  test("outputIsString flag forces 0: even for non-string yields", async () => {
    // When the procedure declares `output: z.string()`, every yield is
    // coerced via String(). Our test config marks the procedure with
    // `outputIsString: true`; the synthetic entry plumbs the flag onto
    // the iterator object so the runtime kernel encodes accordingly.
    //
    // Here in the synthetic-entry path (slow path), the entry itself
    // owns the encoding. So the test exercises the same flag through
    // `fn.config.output = z.string()` detection.
    const fakeStringSchema = { _def: { typeName: "ZodString" } };
    const { fetch, cleanup } = await loadSyntheticEntry({
      exports: {
        async numbers() {
          return (async function* () {
            yield 1;
            yield 2;
          })();
        },
      },
      configs: {
        numbers: { output: fakeStringSchema },
      },
    });
    try {
      const res = await fetch(
        new Request("http://localhost/_zs/v1/numbers", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: "{}",
        }),
      );
      const text = await readBody(res);
      // 1 and 2 are stringified via String(v) → "1", "2" — emitted as `0:`.
      assert.match(text, /0:"1"\n/, `expected 0:"1", got: ${text}`);
      assert.match(text, /0:"2"\n/, `expected 0:"2", got: ${text}`);
      assert.match(text, /d:\{\}\n/, `got: ${text}`);
    } finally {
      await cleanup();
    }
  });
});
