/**
 * list_apps tool — lists all deployed apps on the platform.
 */
import { tool } from "@langchain/core/tools";
import { z } from "zod";

const APPBASE_URL = process.env.APPBASE_URL || "http://localhost:3333";
const APPBASE_KEY = process.env.APPBASE_MASTER_KEY || "dev-master-key";

export const listApps = tool(
  async () => {
    const res = await fetch(`${APPBASE_URL}/api/apps`, {
      headers: { Authorization: `Bearer ${APPBASE_KEY}` },
    });

    if (!res.ok) {
      return `Failed to list apps: ${await res.text()}`;
    }

    const apps = await res.json();

    if (!Array.isArray(apps) || apps.length === 0) {
      return "No apps deployed yet.";
    }

    return apps
      .map(
        (a: any) =>
          `- ${a.id} (plan: ${a.plan_id}, v${a.version}, updated: ${a.updated_at})`
      )
      .join("\n");
  },
  {
    name: "list_apps",
    description: "List all apps currently deployed on the appbase platform.",
    schema: z.object({}),
  }
);
