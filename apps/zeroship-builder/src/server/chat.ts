"use server";
// AI chat — runs deepagents (LangGraph) inside the zeroship V8 runtime.
//
// The vite-plugin's node-compat layer (unenv-based, see
// `sdks/vite-plugin/src/node-compat.ts`) provides shims for every
// `node:*` specifier deepagents + langchain reach for. Anything still
// missing surfaces at build/runtime as a clear error and we add it
// to the polyfills there — the runtime itself stays clean WinterCG.
//
// Dogfood: this module compiles into `dist/server/index.js` and
// runs as part of our own zeroship app's fetch handler. There is no
// out-of-process agent service.

import { createDeepAgent } from "deepagents";
import { ChatOpenAI } from "@langchain/openai";
import { ChatAnthropic } from "@langchain/anthropic";
import { HumanMessage, AIMessage } from "@langchain/core/messages";
import { tool } from "@langchain/core/tools";
import { z } from "zod";
import { OPENAI_API_KEY } from "./env";
import {
  openSession,
  listFiles as sbxListFiles,
  readFile as sbxReadFile,
  writeFile as sbxWriteFile,
  deleteFile as sbxDeleteFile,
  execCommand,
} from "./sandbox";
import {
  createApp as cpCreateApp,
  listApps as cpListApps,
  deployApp as cpDeployApp,
} from "./apps";

// ─── Wire types we expose to the client ──────────────────────────

export interface ChatRequest {
  messages: { role: "user" | "assistant" | "system"; content: string }[];
  context?: { app_id?: string; app_name?: string };
  model?: string;
  provider?: "openai" | "anthropic";
}

export type ChatEvent =
  | { type: "text"; content: string }
  | { type: "tool_start"; name: string; input: unknown }
  | { type: "tool_end"; name: string; output: string; error?: boolean }
  | { type: "done" }
  | { type: "error"; content: string };

// ─── Tool surface ────────────────────────────────────────────────
//
// Every sandbox / control-plane operation is a deepagents tool.
// The `sandbox_*` prefix avoids collision with deepagents' built-in
// virtual-FS tools (`read_file`, `write_file`, `edit_file`, `ls`,
// `execute`) — those operate on agent-state memory; ours operate
// on the real Docker container.

function makeTools(ctx: { app_id?: string; app_name?: string }) {
  const pid = ctx.app_id;

  return [
    tool(
      async ({ project_id }) => {
        const id = project_id ?? pid;
        if (!id) return JSON.stringify({ ok: false, error: "no project_id" });
        const info = await openSession(id);
        return JSON.stringify({ ok: true, ...info });
      },
      {
        name: "open_session",
        description: "Open or attach to the project's sandbox container. Idempotent.",
        schema: z.object({
          project_id: z.string().optional()
            .describe("App UUID. Defaults to workspace context's app_id."),
        }),
      },
    ),
    tool(
      async ({ project_id }) => {
        const id = project_id ?? pid;
        if (!id) return JSON.stringify({ ok: false, error: "no project_id" });
        const entries = await sbxListFiles(id);
        return JSON.stringify({ ok: true, entries });
      },
      {
        name: "sandbox_list_files",
        description: "List files in the project workspace (skips node_modules, .git, dist).",
        schema: z.object({ project_id: z.string().optional() }),
      },
    ),
    tool(
      async ({ project_id, path }) => {
        const id = project_id ?? pid;
        if (!id) return JSON.stringify({ ok: false, error: "no project_id" });
        try {
          const content = await sbxReadFile(id, path);
          return JSON.stringify({ ok: true, path, content });
        } catch (e: any) {
          return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
        }
      },
      {
        name: "sandbox_read_file",
        description:
          "Read a file from the PROJECT WORKSPACE (real Docker container). " +
          "Use this — NOT deepagents' built-in `read_file` (agent-state memory).",
        schema: z.object({
          project_id: z.string().optional(),
          path: z.string(),
        }),
      },
    ),
    tool(
      async ({ project_id, path, content }) => {
        const id = project_id ?? pid;
        if (!id) return JSON.stringify({ ok: false, error: "no project_id" });
        try {
          const r = await sbxWriteFile(id, path, content);
          return JSON.stringify({ ok: true, ...r });
        } catch (e: any) {
          return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
        }
      },
      {
        name: "sandbox_write_file",
        description:
          "Create or overwrite a file in the PROJECT WORKSPACE. 5 MB cap. " +
          "Use this — NOT deepagents' built-in `write_file`.",
        schema: z.object({
          project_id: z.string().optional(),
          path: z.string(),
          content: z.string(),
        }),
      },
    ),
    tool(
      async ({ project_id, path }) => {
        const id = project_id ?? pid;
        if (!id) return JSON.stringify({ ok: false, error: "no project_id" });
        try {
          await sbxDeleteFile(id, path);
          return JSON.stringify({ ok: true });
        } catch (e: any) {
          return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
        }
      },
      {
        name: "sandbox_delete_file",
        description: "Delete a file from the project workspace. Idempotent.",
        schema: z.object({
          project_id: z.string().optional(),
          path: z.string(),
        }),
      },
    ),
    tool(
      async ({ project_id, cmd, cwd, timeout_ms }) => {
        const id = project_id ?? pid;
        if (!id) return JSON.stringify({ ok: false, error: "no project_id" });
        try {
          const out = await execCommand(id, cmd, { cwd, timeoutMs: timeout_ms });
          const cap = (s: string) => s.length > 4000 ? s.slice(0, 4000) + "\n…(truncated)" : s;
          return JSON.stringify({
            ok: out.status === 0,
            status: out.status,
            stdout: cap(out.stdout),
            stderr: cap(out.stderr),
          });
        } catch (e: any) {
          return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
        }
      },
      {
        name: "sandbox_exec",
        description:
          "Run a shell command inside the project's sandbox container. " +
          "Useful for `npm install`, `npm run build`, `git`, etc. Default timeout 60s; max 600s.",
        schema: z.object({
          project_id: z.string().optional(),
          cmd: z.string(),
          cwd: z.string().optional(),
          timeout_ms: z.number().int().positive().max(600_000).optional(),
        }),
      },
    ),
    tool(
      async () => {
        const apps = await cpListApps();
        return JSON.stringify({ ok: true, apps });
      },
      {
        name: "list_apps",
        description: "Enumerate apps on the platform.",
        schema: z.object({}),
      },
    ),
    tool(
      async ({ name }) => {
        try {
          const app = await cpCreateApp(name, "free");
          return JSON.stringify({ ok: true, app_id: app.id, app_name: app.name });
        } catch (e: any) {
          return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
        }
      },
      {
        name: "create_app",
        description: "Create a new zeroship app with a slug name.",
        schema: z.object({
          name: z.string().regex(/^[a-zA-Z0-9_-]+$/).min(1).max(64),
        }),
      },
    ),
    tool(
      async ({ app_id, server_js }) => {
        try {
          const r = await cpDeployApp(app_id, server_js);
          return JSON.stringify({ ok: true, ...r });
        } catch (e: any) {
          return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
        }
      },
      {
        name: "deploy_app",
        description:
          "Deploy a single ES module to an app. The module MUST be " +
          "`export default { fetch(req, env, ctx) { ... } }`.",
        schema: z.object({
          app_id: z.string().uuid(),
          server_js: z.string(),
        }),
      },
    ),
  ];
}

// ─── Model selection ─────────────────────────────────────────────

function pickModel(req: ChatRequest) {
  const proc = (globalThis as any).process;
  const provider = req.provider
    ?? (proc?.env?.OPENAI_API_KEY ? "openai"
        : proc?.env?.ANTHROPIC_API_KEY ? "anthropic"
        : "openai");

  if (provider === "anthropic") {
    return new ChatAnthropic({
      model: req.model ?? "claude-sonnet-4-5-20250929",
      maxTokens: 16_000,
    });
  }
  return new ChatOpenAI({
    model: req.model ?? "gpt-4o",
    apiKey: OPENAI_API_KEY(),
    maxTokens: 16_000,
  });
}

// ─── System prompt ───────────────────────────────────────────────

function systemPrompt(ctx: { app_id?: string; app_name?: string } | undefined): string {
  const ctxBlock = ctx?.app_id
    ? `\n\n## Workspace context\n- app_id (defaults sandbox tools' project_id): ${ctx.app_id}\n- app_name: ${ctx.app_name ?? "<unknown>"}\n`
    : "\n\n## Workspace context\n\nNo app exists yet. Call `create_app` first.\n";

  return `You are the zeroship build agent. You ship working apps from a creator's natural-language description.

## How a zeroship app works

A zeroship app is ONE ES module. You write it as plain top-level
\`export\`s — NO \`default.fetch\`, NO routes, NO URL parsing.

The platform routes:
  POST /apps/<name>/_rpc/<methodName>  → invokes \`user.<methodName>(...args)\`
                                          (body is a JSON array of args)
  GET  /apps/<name>/                   → invokes \`user.index(request)\`
                                          (return value is rendered as HTML)

So you export TWO things:

1. RPC methods — async functions for state mutation / queries.
2. \`index(req)\` — returns the HTML page for the creator's app. The
   page is what the studio's preview iframe will load. It must be a
   complete, useful UI. Inside it, call the RPC methods via
   \`fetch('/apps/<name>/_rpc/<method>', { method: 'POST', headers: {
   'content-type': 'application/json', 'x-api-key': '<key>' },
   body: JSON.stringify([...args]) })\`.

   Use \`req.url\` to derive the app's base URL — it will be something
   like \`https://<name>.zeroship.app\` (or in dev, with /apps/<name>
   prefix). Read the api_key from \`process.env.API_KEY\` (the platform
   injects it).

\`\`\`js
// Example: counter app with a real UI
let count = 0;

export async function get() { return count; }
export async function increment(by) { count += by ?? 1; return count; }

export async function index() {
  return (
    '<!doctype html><html><head><title>Counter</title></head>' +
    '<body><h1 id=v>0</h1><button id=b>+1</button>' +
    '<script>' +
    'const KEY = "' + process.env.API_KEY + '";' +
    'const v = document.getElementById("v");' +
    'const b = document.getElementById("b");' +
    'async function rpc(name, args) {' +
    '  const r = await fetch("./_rpc/" + name, {' +
    '    method: "POST",' +
    '    headers: { "content-type": "application/json", "x-api-key": KEY },' +
    '    body: JSON.stringify(args || [])' +
    '  });' +
    '  return r.json();' +
    '}' +
    'rpc("get", []).then(n => v.textContent = n);' +
    'b.onclick = async () => { v.textContent = await rpc("increment", [1]); };' +
    '</script></body></html>'
  );
}
\`\`\`

Persist state in module-scoped variables. Throwing \`Error\` returns
500. Throwing \`Object.assign(new Error('...'), { status: 404 })\`
returns the given status.

## Tools

\`deploy_app\` is the only tool you NEED for most apps:
- \`deploy_app(app_id, server_js)\` — pushes the ES module to the
   workspace's app and the platform reloads it.

The sandbox tools are available for complex apps but PREFER a single
self-contained module via deploy_app. Most apps don't need anything else.

## Critical rules

1. **The deployed module is a single ES file parsed by V8. Avoid nested
   backtick template literals** — an inner \\\` inside an outer template
   literal terminates the outer one and breaks parsing. Build inline
   HTML/JS as single- or double-quoted strings with \`+\` concatenation.

2. **Always include an \`index()\` export.** Without it, the studio's
   preview iframe shows 404. Even a 30-line HTML page that renders a
   form + result is enough.

3. **Wire the page to the RPC methods.** The HTML you return from
   \`index()\` should call your RPC methods via \`fetch\`, so users can
   actually drive the app from the browser.${ctxBlock}`;
}

// ─── Entry point ─────────────────────────────────────────────────

export async function* chat(req: ChatRequest): AsyncGenerator<ChatEvent> {
  const proc = (globalThis as any).process;
  if (!OPENAI_API_KEY() && !proc?.env?.ANTHROPIC_API_KEY) {
    yield { type: "error", content: "no LLM key set (OPENAI_API_KEY or ANTHROPIC_API_KEY)" };
    yield { type: "done" };
    return;
  }

  const ctx = req.context ?? {};
  let agent;
  try {
    agent = createDeepAgent({
      model: pickModel(req),
      systemPrompt: systemPrompt(ctx),
      tools: makeTools(ctx),
    });
  } catch (err: any) {
    yield { type: "error", content: `agent init failed: ${err?.message ?? String(err)}` };
    yield { type: "done" };
    return;
  }

  const lcMessages = req.messages.map((m) =>
    m.role === "user" ? new HumanMessage(m.content) : new AIMessage(m.content),
  );

  try {
    const eventStream = agent.streamEvents(
      { messages: lcMessages },
      { configurable: { thread_id: ctx.app_id ?? "default" }, version: "v2" },
    );

    for await (const event of eventStream as AsyncIterable<any>) {
      if (event.event === "on_chat_model_stream" && event.data?.chunk?.content) {
        const content = event.data.chunk.content;
        if (typeof content === "string" && content.length > 0) {
          yield { type: "text", content };
        } else if (Array.isArray(content)) {
          for (const block of content) {
            if (block.type === "text" && block.text) {
              yield { type: "text", content: block.text };
            }
          }
        }
      }
      if (event.event === "on_tool_start") {
        yield { type: "tool_start", name: String(event.name), input: event.data?.input };
      }
      if (event.event === "on_tool_end") {
        const out = event.data?.output;
        const stringified = typeof out === "string" ? out : JSON.stringify(out);
        let isError = false;
        if (typeof out === "string") {
          try {
            const parsed = JSON.parse(out);
            if (parsed && parsed.ok === false) isError = true;
          } catch {}
        }
        yield { type: "tool_end", name: String(event.name), output: stringified, error: isError };
      }
    }

    yield { type: "done" };
  } catch (err: any) {
    yield { type: "error", content: err?.message ?? String(err) };
    yield { type: "done" };
  }
}
