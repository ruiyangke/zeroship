/**
 * create_app tool — provisions a new zeroship app and returns its
 * UUID + name. The agent calls this once per project; subsequent
 * deploys hit the same UUID.
 */
import { tool } from "@langchain/core/tools";
import { z } from "zod";
import { CONTROL_URL, CONTROL_KEY } from "../env.js";

export const createApp = tool(
  async ({ name, plan_id }) => {
    const res = await fetch(`${CONTROL_URL}/api/apps`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${CONTROL_KEY}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ name, plan_id: plan_id ?? "free" }),
    });

    if (!res.ok) {
      return `Failed to create app: HTTP ${res.status} — ${await res.text()}`;
    }

    const app = await res.json();
    return JSON.stringify({
      ok: true,
      app_id: app.id,
      app_name: app.name,
      preview_url: `/apps/${app.name}/`,
      message: `App "${app.name}" created (id=${app.id}). Deploy code to it next via deploy_app.`,
    });
  },
  {
    name: "create_app",
    description:
      "Create a new zeroship app. Returns the app's UUID, slug name, and the preview URL. Use this exactly once at the start of a project, before deploy_app.",
    schema: z.object({
      name: z
        .string()
        .min(1)
        .max(64)
        .regex(/^[a-zA-Z0-9_-]+$/, "alphanumeric, dashes, and underscores only")
        .describe(
          "Slug for the app (e.g. 'recipe-app'). Becomes part of the URL and must be globally unique.",
        ),
      plan_id: z
        .string()
        .optional()
        .describe("Plan id; default 'free'."),
    }),
  },
);
