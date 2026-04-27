/**
 * list_apps tool — enumerate all deployed apps on the platform.
 * Lets the agent discover existing projects when the user asks to
 * resume work on something they've built before.
 */
import { tool } from "@langchain/core/tools";
import { z } from "zod";
import { CONTROL_URL, CONTROL_KEY } from "../env.js";

export const listApps = tool(
  async () => {
    const res = await fetch(`${CONTROL_URL}/api/apps`, {
      headers: { Authorization: `Bearer ${CONTROL_KEY}` },
    });

    if (!res.ok) {
      return `Failed to list apps: HTTP ${res.status} — ${await res.text()}`;
    }

    const apps = await res.json();

    if (!Array.isArray(apps) || apps.length === 0) {
      return "No apps deployed yet.";
    }

    return apps
      .map(
        (a: any) =>
          `- ${a.name} (id=${a.id}, plan=${a.plan_id}, deployed=${a.deploy_hash ? "yes" : "no"})`,
      )
      .join("\n");
  },
  {
    name: "list_apps",
    description:
      "List every app on the platform (id, name, plan, whether code is deployed). Useful when the user references an existing app by name.",
    schema: z.object({}),
  },
);
