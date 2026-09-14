/**
 * Storybook MCP middleware — wires `@storybook/mcp`'s HTTP handler into
 * the Storybook dev server at `/mcp`. AI agents can then call the
 * standard Storybook MCP tools (`list-all-documentation`,
 * `get-documentation`, `get-story-documentation`) while iterating on
 * stories live.
 *
 * Storybook 8 doesn't emit the v9-style `manifests/components.json`,
 * so we synthesize a minimal manifest from the on-disk story files
 * under `src/stories/`. The component IDs follow Storybook's slug
 * convention (`components-button`, etc.) so they match the story IDs
 * the runner reports. When SB 8 ships a built-in manifest generator
 * — or we move to SB 9 — this synthesizer can be replaced by the
 * default origin-based provider.
 */

import { readdir, readFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type { Connect } from "vite";
import { createStorybookMcpHandler } from "@storybook/mcp";

const here = dirname(fileURLToPath(import.meta.url));
const storiesDir = resolve(here, "../src/stories");

type StoryDescriptor = {
  id: string;
  name: string;
};

type ComponentEntry = {
  id: string;
  name: string;
  path: string;
  description?: string;
  stories?: StoryDescriptor[];
};

/* Storybook auto-titles stories under `Components/<Name>` (set via the
 * `title` field in every story meta). Slugify exactly the same way
 * Storybook does (lowercase, non-alnum → `-`). */
function slugify(value: string): string {
  return value
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

function parseStoryFile(
  fileName: string,
  source: string,
): ComponentEntry | null {
  const titleMatch = source.match(/title:\s*["'`]([^"'`]+)["'`]/);
  if (!titleMatch) return null;
  const title = titleMatch[1];
  const slug = slugify(title);
  const name = fileName.replace(/\.stories\.tsx?$/, "");

  /* Stories are either `export const Foo: Story = { … }` or
   * `export const Foo = { … } satisfies Story`. The exact regex
   * doesn't need to be perfect — a few false negatives are fine for a
   * starter manifest. The Test Runner is the source of truth for what
   * actually runs. */
  const storyNames = Array.from(
    source.matchAll(/export\s+const\s+(\w+)\s*:\s*Story\b/g),
  ).map((match) => match[1]);

  const stories: StoryDescriptor[] = storyNames.map((storyName) => ({
    id: `${slug}--${slugify(storyName)}`,
    name: storyName,
  }));

  return {
    id: slug,
    name,
    path: `src/stories/${fileName}`,
    stories,
  };
}

async function buildComponentManifest(): Promise<string> {
  const components: Record<string, ComponentEntry> = {};
  let entries: string[] = [];
  try {
    entries = await readdir(storiesDir);
  } catch {
    return JSON.stringify({ v: 1, components });
  }
  for (const file of entries) {
    if (!/\.stories\.tsx?$/.test(file)) continue;
    const source = await readFile(join(storiesDir, file), "utf8");
    const parsed = parseStoryFile(file, source);
    if (!parsed) continue;
    components[parsed.id] = parsed;
  }
  return JSON.stringify({ v: 1, components });
}

/**
 * Build a Vite/Connect middleware that handles `GET|POST /mcp`. The
 * Storybook MCP handler is fetch-shaped (`(Request) => Promise<Response>`),
 * so we adapt Node IncomingMessage / ServerResponse to/from WHATWG
 * Request / Response.
 */
export async function createStorybookMcpMiddleware(): Promise<
  Connect.NextHandleFunction
> {
  let cachedManifest: string | null = null;

  const handler = await createStorybookMcpHandler({
    manifestProvider: async (_request, path) => {
      if (path.endsWith("components.json")) {
        cachedManifest ??= await buildComponentManifest();
        return cachedManifest;
      }
      /* No docs manifest yet — return an empty docs map so the handler
       * doesn't 500 when an agent asks for docs. */
      if (path.endsWith("docs.json")) {
        return JSON.stringify({ v: 1, docs: {} });
      }
      throw new Error(`Unknown manifest path: ${path}`);
    },
  });

  return async function storybookMcpMiddleware(req, res, next) {
    /* The Storybook MCP handler claims `/mcp`; everything else falls
     * through to Storybook's normal pipeline. */
    const url = req.url ?? "";
    if (!url.startsWith("/mcp")) return next();

    try {
      const protocol = (req.socket as { encrypted?: boolean }).encrypted
        ? "https"
        : "http";
      const host = req.headers.host ?? "127.0.0.1:6006";
      const fullUrl = new URL(url, `${protocol}://${host}`);
      const method = req.method ?? "GET";
      const headers = new Headers();
      for (const [name, value] of Object.entries(req.headers)) {
        if (Array.isArray(value)) {
          for (const v of value) headers.append(name, v);
        } else if (typeof value === "string") {
          headers.set(name, value);
        }
      }

      /* Buffer the body for POST/PUT/DELETE. Storybook's dev server
       * never sees giant payloads here; MCP messages are JSON. */
      let body: Uint8Array | undefined;
      if (method !== "GET" && method !== "HEAD") {
        const chunks: Buffer[] = [];
        for await (const chunk of req) {
          chunks.push(chunk as Buffer);
        }
        if (chunks.length > 0) {
          body = Buffer.concat(chunks);
        }
      }

      const request = new Request(fullUrl, {
        method,
        headers,
        body: body as BodyInit | undefined,
        duplex: "half",
      } as RequestInit);

      const response = await handler(request);
      res.statusCode = response.status;
      response.headers.forEach((value, name) => {
        res.setHeader(name, value);
      });
      if (response.body) {
        const reader = response.body.getReader();
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          res.write(value);
        }
      }
      res.end();
    } catch (err) {
      res.statusCode = 500;
      res.setHeader("content-type", "application/json");
      res.end(
        JSON.stringify({
          error: err instanceof Error ? err.message : String(err),
        }),
      );
    }
  };
}
