// Vite config for the zeroship-builder app.
//
// `@zeroship/vite-plugin` does the heavy lifting:
//   - server-function transform: any module starting with `"use server"`
//     gets compiled into RPC stubs the client side can call as plain
//     async functions, while the server-side code is bundled into the
//     fetch handler that ships in the .appbundle.
//   - dev mode: spins a tiny zeroship runtime in-process so calls hit
//     the real server-functions during `vite dev`.
//   - build mode: emits dist/ + zeroship.appbundle.
import { defineConfig, loadEnv, type PluginOption, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { zeroship } from "@zeroship/vite-plugin";
import path from "node:path";
import type { ServerResponse } from "node:http";

// ---------------------------------------------------------------------------
// Mock /_rpc/postChat middleware (Node.js dev only)
//
// The zeroship V8 runtime's ModuleRunner initialization is unreliable in
// some environments. For e2e tests we serve the mock postChat stream
// directly from Vite's Node.js process, bypassing the runtime entirely.
//
// This middleware ONLY activates when ZEROSHIP_MOCK_CHAT=1 is set, so
// it never interferes with real development or production builds.
// ---------------------------------------------------------------------------

type AIStreamChunk =
  | { type: "text-delta"; delta: string }
  | { type: "tool-call"; toolCallId: string; toolName: string; args: unknown }
  | { type: "tool-result"; toolCallId: string; result: unknown }
  | { type: "data-part"; partName: string; payload: unknown }
  | { type: "error"; message: string }
  | { type: "finish"; usage?: { inputTokens?: number; outputTokens?: number } };

function encodeChunk(chunk: AIStreamChunk): string {
  return JSON.stringify(chunk) + "\n";
}

async function* mockGenerate(): AsyncIterable<AIStreamChunk> {
  const preamble = "Got it — let me think about that.\n\n";
  for (const ch of preamble) {
    yield { type: "text-delta", delta: ch };
    await new Promise((r) => setTimeout(r, 5));
  }
  yield {
    type: "data-part",
    partName: "survey",
    payload: {
      preamble: "A quick thing first:",
      questions: [
        {
          id: "vibe",
          prompt: "Vibe?",
          kind: {
            type: "single_choice",
            options: [
              { value: "cozy", label: "cozy / warm" },
              { value: "minimal", label: "minimal" },
              { value: "playful", label: "playful" },
            ],
          },
          default: "minimal",
        },
      ],
      skip_label: "skip — just build",
    },
  };
  await new Promise((r) => setTimeout(r, 50));
  const resume = "\nOK — I'll start by writing a small file.\n\n";
  for (const ch of resume) {
    yield { type: "text-delta", delta: ch };
    await new Promise((r) => setTimeout(r, 5));
  }
  const toolCallId = "tool_mock_001";
  yield {
    type: "tool-call",
    toolCallId,
    toolName: "write_file",
    args: { path: "src/index.tsx", contents_preview: "<… mock contents …>" },
  };
  yield { type: "tool-result", toolCallId, result: { ok: true, bytes_written: 312 } };
  yield {
    type: "data-part",
    partName: "diff",
    payload: {
      path: "src/index.tsx",
      before: "",
      after: "import { render } from 'react-dom';\nrender(<h1>Hello</h1>, document.body);\n",
    },
  };
  yield {
    type: "data-part",
    partName: "critic-round",
    payload: { round: 1, total: 3, approved: true, issues: [] },
  };
  const trailer = "\nDone. (Plan 01 mock)\n";
  for (const ch of trailer) {
    yield { type: "text-delta", delta: ch };
    await new Promise((r) => setTimeout(r, 5));
  }
  yield { type: "finish" };
}

function mockChatPlugin(): Plugin {
  return {
    name: "zeroship:mock-chat",
    configureServer(server) {
      // Return a function to register as a pre-middleware (runs before Vite's
      // SPA fallback so the path isn't swallowed by index.html serving).
      return () => {
        server.middlewares.use(async (req, res: ServerResponse, next) => {
          if (req.url !== "/_rpc/postChat" || req.method !== "POST") {
            return next();
          }
          res.writeHead(200, {
            "Content-Type": "text/plain; charset=utf-8",
            "Cache-Control": "no-cache, no-transform",
            "X-Accel-Buffering": "no",
            "Access-Control-Allow-Origin": "*",
          });
          try {
            for await (const chunk of mockGenerate()) {
              if (res.destroyed) break;
              res.write(encodeChunk(chunk));
            }
          } catch (_) {
            // Client disconnected (stop button) — that's expected.
          }
          res.end();
        });
      };
    },
  };
}

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "VITE_");
  // Cast each plugin to `PluginOption` so TS's structural-comparison
  // doesn't spiral when Vite + tailwind + zeroship plugins share
  // identically-shaped (but distinct) Plugin types.
  const plugins: PluginOption[] = [
    react() as unknown as PluginOption,
    tailwindcss() as unknown as PluginOption,
    // Mock /_rpc/postChat when running e2e tests (avoids V8 runtime dependency).
    ...(process.env.ZEROSHIP_MOCK_CHAT === "1"
      ? [mockChatPlugin() as unknown as PluginOption]
      : []),
    zeroship({
      // Allow overriding the dev API port so worktrees can run
      // alongside the main dev server (which already owns :3001).
      devServerPort: process.env.ZEROSHIP_DEV_PORT
        ? Number(process.env.ZEROSHIP_DEV_PORT)
        : undefined,
    }) as unknown as PluginOption,
  ];
  return {
    plugins,
    resolve: {
      alias: {
        "@": path.resolve(import.meta.dirname, "./src/client"),
      },
    },
    server: {
      // While developing locally we proxy to the same control / sandbox
      // services the dashboard talks to — the deployed build proxies
      // identically through env vars on the deployed app.
      proxy: {
        "/api/control": env.VITE_PROXY_CONTROL ?? "http://localhost:9090",
        // The preview iframe in the workspace points at `/apps/<name>/`
        // which is path-style routing on the production gateway. In dev
        // we forward those to the local zeroship-gate so the iframe
        // shows the live deployed app instead of vite's SPA fallback.
        "/apps": {
          target: env.VITE_PROXY_GATEWAY ?? "http://localhost:8001",
          changeOrigin: true,
        },
      },
    },
  };
});
