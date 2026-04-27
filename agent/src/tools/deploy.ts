/**
 * deploy_app tool — pushes a JS bundle to an existing zeroship app.
 *
 * Apps are identified by UUID (returned by create_app). Body is the
 * raw module source — the agent should produce the new fetch-handler
 * shape: `export default { fetch(req, env, ctx) { ... } }`.
 *
 * Returns a JSON-serialized result the LLM can parse to confirm the
 * deploy hash + preview URL it should reload.
 */
import { tool } from "@langchain/core/tools";
import { z } from "zod";
import { CONTROL_URL, CONTROL_KEY } from "../env.js";

export const deployApp = tool(
  async ({ app_id, server_js }) => {
    if (!server_js.includes("export default") || !server_js.includes("fetch")) {
      return JSON.stringify({
        ok: false,
        error:
          "server_js must use the fetch-handler shape: `export default { fetch(req, env, ctx) { ... } }`. Don't deploy single named exports — they are no longer supported.",
      });
    }

    const res = await fetch(`${CONTROL_URL}/api/apps/${app_id}/deploy`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${CONTROL_KEY}`,
        "Content-Type": "application/javascript",
      },
      body: server_js,
    });

    if (!res.ok) {
      return JSON.stringify({
        ok: false,
        error: `HTTP ${res.status}: ${await res.text()}`,
      });
    }

    const data = await res.json();

    // Hint the chat layer that this turn ended in a deploy — the
    // dashboard listens for `tool_end` on this name to reload the
    // preview iframe.
    return JSON.stringify({
      ok: true,
      app_id,
      deploy_hash: data.deploy_hash,
      message: `Deployed app ${app_id}. The preview iframe will refresh; ask the user to verify the live result.`,
    });
  },
  {
    name: "deploy_app",
    description:
      "Deploy JavaScript source code to an existing zeroship app. The code MUST be a single ES module that exports a default object with a `fetch(request, env, ctx)` method that returns a Response. Always pass the UUID returned by create_app, not the slug name.",
    schema: z.object({
      app_id: z
        .string()
        .uuid()
        .describe("UUID of the target app (returned by create_app)."),
      server_js: z
        .string()
        .describe(
          "The complete server.js source code. Must be `export default { fetch(req, env, ctx) { ... } }`.",
        ),
    }),
  },
);
