/**
 * fetch_app_code tool — read the currently-deployed source for an
 * app. Useful for iterations: the agent reads what's there, applies
 * a change, and redeploys. Without this, every iteration regenerates
 * from scratch (slow, drifts, hallucinates).
 */
import { tool } from "@langchain/core/tools";
import { z } from "zod";
import { CONTROL_URL, CONTROL_KEY } from "../env.js";

export const fetchAppCode = tool(
  async ({ app_id }) => {
    const res = await fetch(`${CONTROL_URL}/api/apps/${app_id}`, {
      headers: { Authorization: `Bearer ${CONTROL_KEY}` },
    });

    if (!res.ok) {
      return JSON.stringify({
        ok: false,
        error: `HTTP ${res.status}: ${await res.text()}`,
      });
    }

    const app = await res.json();
    if (typeof app.server_js !== "string") {
      return JSON.stringify({
        ok: true,
        app_id,
        app_name: app.name,
        server_js: "",
        message:
          "App has no code deployed yet. Generate fresh code and call deploy_app.",
      });
    }

    return JSON.stringify({
      ok: true,
      app_id,
      app_name: app.name,
      server_js: app.server_js,
    });
  },
  {
    name: "fetch_app_code",
    description:
      "Fetch the currently-deployed source for an app. Use this BEFORE iterating on an existing project so you edit the actual deployed code instead of regenerating from memory.",
    schema: z.object({
      app_id: z.string().uuid().describe("UUID of the app to read."),
    }),
  },
);
