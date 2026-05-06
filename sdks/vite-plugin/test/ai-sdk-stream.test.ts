/**
 * Synthetic-entry SSE encoding.
 *
 * The synthetic SSR entry's `_zsFetch` path catches handlers that
 * return an async iterator and frames them per the Vercel AI-SDK Data
 * Stream Protocol:
 *
 *   `0:"text"\n`     string yields
 *   `2:[<json>]\n`   object yields
 *   `e:{...}\n`      mid-stream error envelope (zeroship extension)
 *   `d:{}\n`         done
 *
 * These tests instantiate the synthetic entry source as a real ESM
 * module via dynamic import. Each test stubs the user module, then
 * invokes `default.fetch` with a synthetic Request and reads the
 * response stream as text.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { writeFile, mkdtemp, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";

import { buildServerEntrySource } from "../src/rpc-registry.js";

interface UserModuleConfig {
  /** Map of exported function names to their implementations. */
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  exports: Record<string, (...args: any[]) => any>;
  /** Optional config blocks attached as `<fn>.config`. */
  configs?: Record<string, Record<string, unknown>>;
  /** Optional default export — fetch handler. */
  defaultFetch?: (req: Request) => Promise<Response> | Response;
}

/**
 * Materialize the synthetic entry plus a stub user module, then return
 * the dynamically-imported entry's `default.fetch`.
 */
async function loadSyntheticEntry(user: UserModuleConfig): Promise<{
  fetch: (req: Request) => Promise<Response>;
  cleanup: () => Promise<void>;
}> {
  const dir = await mkdtemp(join(tmpdir(), "zsrpc-aisdk-"));

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

  const userPath = join(dir, "user.mjs");
  const userSrc = `const __zsTest = globalThis[${JSON.stringify(userKey)}];
${userExportsSrc}
${defaultExportSrc}
`;
  await writeFile(userPath, userSrc, "utf8");

  // Build the synthetic entry source pointing at the user module.
  const procedures = fnNames.map((n) => ({
    filePath: pathToFileURL(userPath).href,
    exportName: n,
    wireId: (user.configs?.[n]?.id as string | undefined) ?? n,
  }));
  const entrySrc = buildServerEntrySource({
    userEntryRel: pathToFileURL(userPath).href,
    procedures,
  });
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
      assert.match(text, /2:\[\{"id":1\}\]\n/, `got: ${text}`);
      const eMatch = text.match(/e:(\{.*?\})\n/);
      assert.ok(eMatch, `e: envelope missing in ${text}`);
      const env = JSON.parse(eMatch![1]);
      assert.equal(env.message, "upstream gone");
      assert.equal(env.code, "UNAVAILABLE");
      assert.deepEqual(env.details, { hint: "demo" });
      assert.equal(env.retryable, true);
      const eIdx = text.indexOf("e:");
      assert.match(text.slice(eIdx), /d:\{\}\n/, `expected d:{} after error: ${text}`);
    } finally {
      await cleanup();
    }
  });

  test("outputIsString flag forces 0: even for non-string yields", async () => {
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
      assert.match(text, /0:"1"\n/, `expected 0:"1", got: ${text}`);
      assert.match(text, /0:"2"\n/, `expected 0:"2", got: ${text}`);
      assert.match(text, /d:\{\}\n/, `got: ${text}`);
    } finally {
      await cleanup();
    }
  });
});
