"use server";
// AI chat — vanilla OpenAI tool-use loop in pure JS.
//
// No LangGraph, no deepagents — those drag Node-only deps that the
// V8 runtime can't host. This implements the tool-use loop directly:
//
//   user msg → POST /v1/chat/completions
//   if reply has tool_calls:
//     run each tool (sandbox + control-plane proxies)
//     append tool result messages
//     loop
//   else:
//     return text
//
// Streamed via an async generator — the React side iterates it with
// `for await`. The vite-plugin transforms the generator into a chunked
// stream over the RPC wire automatically.
//
// Total cost: ~250 lines for what deepagents charges 5MB of bundle.

import { OPENAI_API_KEY } from "./env";
import {
  openSession,
  listFiles,
  readFile,
  writeFile,
  deleteFile,
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
}

export type ChatEvent =
  | { type: "text"; content: string }
  | { type: "tool_start"; name: string; input: unknown }
  | { type: "tool_end"; name: string; output: string; error?: boolean }
  | { type: "done" }
  | { type: "error"; content: string };

// ─── Tool definitions ────────────────────────────────────────────
//
// We keep tool surface small — the bigger the surface, the more
// confused gpt-4o gets. These six cover everything the AI builder
// needs for vite + react + deploy.

interface ToolDef {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
  run(args: any, ctx: { app_id?: string; app_name?: string }): Promise<string>;
}

const TOOLS: ToolDef[] = [
  {
    name: "open_session",
    description:
      "Open or attach to the project's sandbox container. Returns session_id, container_ip, workspace_path. Idempotent.",
    parameters: {
      type: "object",
      properties: {
        project_id: { type: "string", description: "App UUID from the workspace context." },
      },
      required: ["project_id"],
    },
    async run(args) {
      const info = await openSession(args.project_id);
      return JSON.stringify({ ok: true, ...info });
    },
  },
  {
    name: "list_files",
    description: "Walk the project workspace; returns a list of files + sizes (skips node_modules, .git, dist).",
    parameters: {
      type: "object",
      properties: { project_id: { type: "string" } },
      required: ["project_id"],
    },
    async run(args) {
      const entries = await listFiles(args.project_id);
      return JSON.stringify({ ok: true, entries });
    },
  },
  {
    name: "read_file",
    description: "Read a file from the project workspace. Path is relative to the workspace root.",
    parameters: {
      type: "object",
      properties: {
        project_id: { type: "string" },
        path: { type: "string" },
      },
      required: ["project_id", "path"],
    },
    async run(args) {
      try {
        const content = await readFile(args.project_id, args.path);
        return JSON.stringify({ ok: true, path: args.path, content });
      } catch (e: any) {
        return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
      }
    },
  },
  {
    name: "write_file",
    description:
      "Create or overwrite a file in the project workspace. Parent directories are created automatically. 5 MB cap.",
    parameters: {
      type: "object",
      properties: {
        project_id: { type: "string" },
        path: { type: "string" },
        content: { type: "string" },
      },
      required: ["project_id", "path", "content"],
    },
    async run(args) {
      try {
        const r = await writeFile(args.project_id, args.path, args.content);
        return JSON.stringify({ ok: true, ...r });
      } catch (e: any) {
        return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
      }
    },
  },
  {
    name: "delete_file",
    description: "Delete a file from the project workspace. Idempotent.",
    parameters: {
      type: "object",
      properties: {
        project_id: { type: "string" },
        path: { type: "string" },
      },
      required: ["project_id", "path"],
    },
    async run(args) {
      try {
        await deleteFile(args.project_id, args.path);
        return JSON.stringify({ ok: true });
      } catch (e: any) {
        return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
      }
    },
  },
  {
    name: "exec",
    description:
      "Run a shell command inside the sandbox container. Useful for `npm install`, `npm run build`, `git ...`. Default cwd /workspace; default timeout 60s; max 600s. Output truncated at 4 KB per stream.",
    parameters: {
      type: "object",
      properties: {
        project_id: { type: "string" },
        cmd: { type: "string" },
        cwd: { type: "string" },
        timeout_ms: { type: "integer" },
      },
      required: ["project_id", "cmd"],
    },
    async run(args) {
      try {
        const out = await execCommand(args.project_id, args.cmd, {
          cwd: args.cwd, timeoutMs: args.timeout_ms,
        });
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
  },
  {
    name: "list_apps",
    description: "List every zeroship app on the platform (id, name, plan, deploy state).",
    parameters: { type: "object", properties: {} },
    async run() {
      const apps = await cpListApps();
      return JSON.stringify({ ok: true, apps });
    },
  },
  {
    name: "create_app",
    description: "Create a new zeroship app with a slug name. Returns the app's UUID + slug.",
    parameters: {
      type: "object",
      properties: { name: { type: "string", description: "Slug (alphanumeric/dash/underscore, 1-64 chars)." } },
      required: ["name"],
    },
    async run(args) {
      try {
        const app = await cpCreateApp(args.name, "free");
        return JSON.stringify({ ok: true, app_id: app.id, app_name: app.name });
      } catch (e: any) {
        return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
      }
    },
  },
  {
    name: "deploy_app",
    description:
      "Deploy a single ES module to an app. Module MUST be `export default { fetch(req, env, ctx) { ... } }`. " +
      "For React/Vite projects use the build_and_publish flow on the agent host instead — this tool is only " +
      "for tiny single-file backends.",
    parameters: {
      type: "object",
      properties: {
        app_id: { type: "string" },
        server_js: { type: "string" },
      },
      required: ["app_id", "server_js"],
    },
    async run(args) {
      try {
        const r = await cpDeployApp(args.app_id, args.server_js);
        return JSON.stringify({ ok: true, ...r });
      } catch (e: any) {
        return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
      }
    },
  },
];

const TOOL_BY_NAME = new Map(TOOLS.map((t) => [t.name, t]));

// ─── System prompt ───────────────────────────────────────────────

function systemPrompt(ctx: { app_id?: string; app_name?: string } | undefined): string {
  const ctxBlock = ctx?.app_id
    ? `\n\n## Workspace context\n- app_id: ${ctx.app_id}\n- app_name: ${ctx.app_name ?? "<unknown>"}\n`
    : "\n\n## Workspace context\n\nNo app exists yet. Call `create_app` first.\n";
  return `You are an expert app developer for the zeroship platform. You build complete, working full-stack apps from a creator's natural-language description.

## Architecture

The project workspace lives in a Docker sandbox container with node, npm, git, vite. Use these tools to manipulate it:

- \`open_session(project_id)\`     start/attach the sandbox session
- \`list_files(project_id)\`       walk the workspace
- \`read_file(project_id, path)\`  read a file
- \`write_file(project_id, path, content)\`  create/overwrite a file
- \`delete_file(project_id, path)\` remove a file
- \`exec(project_id, cmd)\`        run a shell command (npm install, npm run build, git, …)

For platform-level operations:
- \`list_apps()\`                   enumerate apps on the platform
- \`create_app(name)\`              provision a new app
- \`deploy_app(app_id, server_js)\` push a single ES-module fetch handler

The starter project is Vite + React + Tailwind. Edit \`src/App.tsx\` for the main component, \`index.html\` for the shell. Use \`exec\` to install deps and run \`npm run build\`.

## Workflow

1. \`open_session\` once per conversation.
2. \`read_file\` before editing — never blind-overwrite.
3. \`write_file\` with focused, surgical edits.
4. \`exec npm install\` once if package.json changes.
5. \`exec npm run build\`.
6. Tell the user the preview URL.${ctxBlock}`;
}

// ─── The loop ────────────────────────────────────────────────────

const MODEL = "gpt-4o";
const MAX_TURNS = 15;

interface OAIMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string | null;
  name?: string;
  tool_calls?: Array<{
    id: string;
    type: "function";
    function: { name: string; arguments: string };
  }>;
  tool_call_id?: string;
}

/** Async generator: yields ChatEvents as the loop progresses.
 *  React side: `for await (const ev of chat({...})) { ... }`. */
export async function* chat(req: ChatRequest): AsyncGenerator<ChatEvent> {
  const apiKey = OPENAI_API_KEY();
  if (!apiKey) {
    yield { type: "error", content: "OPENAI_API_KEY not set on the deployed app" };
    yield { type: "done" };
    return;
  }

  const ctx = req.context;
  const conversation: OAIMessage[] = [
    { role: "system", content: systemPrompt(ctx) },
    ...req.messages.map((m) => ({ role: m.role, content: m.content })),
  ];

  for (let turn = 0; turn < MAX_TURNS; turn++) {
    const res = await fetch("https://api.openai.com/v1/chat/completions", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "authorization": `Bearer ${apiKey}`,
      },
      body: JSON.stringify({
        model: MODEL,
        messages: conversation,
        tools: TOOLS.map((t) => ({
          type: "function",
          function: { name: t.name, description: t.description, parameters: t.parameters },
        })),
        tool_choice: "auto",
      }),
    });

    if (!res.ok) {
      const body = await res.text();
      yield { type: "error", content: `openai → ${res.status}: ${body.slice(0, 500)}` };
      yield { type: "done" };
      return;
    }

    const data = await res.json();
    const msg = data.choices?.[0]?.message as OAIMessage | undefined;
    if (!msg) {
      yield { type: "error", content: "openai response missing message" };
      yield { type: "done" };
      return;
    }

    // Reply text — surface incrementally. Even though we don't (yet)
    // hit the streaming endpoint, we yield the full text in one chunk
    // so the React side has a uniform `for await` loop.
    if (msg.content) {
      yield { type: "text", content: msg.content };
    }

    // No tool calls → final answer reached.
    if (!msg.tool_calls || msg.tool_calls.length === 0) {
      yield { type: "done" };
      return;
    }

    // Append the assistant message + each tool result.
    conversation.push({
      role: "assistant",
      content: msg.content,
      tool_calls: msg.tool_calls,
    });

    for (const call of msg.tool_calls) {
      const tool = TOOL_BY_NAME.get(call.function.name);
      let parsed: any = {};
      try { parsed = JSON.parse(call.function.arguments || "{}"); } catch {}

      // Auto-inject project_id from the workspace context if the tool
      // wants one and the LLM forgot to pass it.
      if (
        tool?.parameters &&
        typeof tool.parameters === "object" &&
        (tool.parameters as any).properties?.project_id &&
        !parsed.project_id &&
        ctx?.app_id
      ) {
        parsed.project_id = ctx.app_id;
      }

      yield { type: "tool_start", name: call.function.name, input: parsed };

      let output = "";
      let isError = false;
      if (!tool) {
        output = JSON.stringify({ ok: false, error: `unknown tool: ${call.function.name}` });
        isError = true;
      } else {
        try {
          output = await tool.run(parsed, ctx ?? {});
          try {
            const j = JSON.parse(output);
            if (j && j.ok === false) isError = true;
          } catch {}
        } catch (e: any) {
          output = JSON.stringify({ ok: false, error: e?.message ?? String(e) });
          isError = true;
        }
      }

      yield { type: "tool_end", name: call.function.name, output, error: isError };

      conversation.push({
        role: "tool",
        tool_call_id: call.id,
        name: call.function.name,
        content: output,
      });
    }
  }

  yield { type: "error", content: `agent stopped after ${MAX_TURNS} turns without producing a final answer` };
  yield { type: "done" };
}
