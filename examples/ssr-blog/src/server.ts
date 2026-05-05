"use server";
// Server entry — picked up by `@zeroship/vite-plugin` and bundled into
// `dist/server/index.js`. The plugin appends the prelude (always) and
// the dispatchRpc/default.fetch bootstrap (ONLY when this file has no
// own `export default`). Since we DO export default below, the
// bootstrap is skipped and our `default.fetch` is the runtime entry.
//
// See README. Real per-request SSR: every GET hits this fetch handler
// and gets back a freshly-rendered HTML page.

import { query } from "@zeroship/server";
import { renderToString } from "react-dom/server";
import { createElement } from "react";
import { App } from "./components/App";
import { POSTS } from "./components/posts";
// The Vite client manifest, exposed by the vite-plugin as a virtual
// module. Inlined into the SSR bundle at build time so the rendered
// HTML loads the right hashed entry filename. When the client build
// hasn't run yet (dev, SSR-only), the virtual module returns `{}` and
// the lookup below falls back to the dev-path fallback.
import clientManifest from "virtual:zeroship/client-manifest";

/** Resolve the hashed URL for a Vite source path; fall back to the dev path. */
function clientScriptTag(srcEntry: string): string {
  const entry = clientManifest[srcEntry];
  if (entry) return `<script type="module" src="/${entry.file}"></script>`;
  // Dev / no-manifest fallback: ship the source path; Vite serves it
  // through the dev middleware.
  return `<script type="module" src="/${srcEntry}"></script>`;
}

/** Inject CSS the client entry imports so the SSR'd HTML doesn't FOUC. */
function clientStylesheet(srcEntry: string): string {
  const entry = clientManifest[srcEntry];
  if (!entry?.css) return "";
  return entry.css.map((p) => `<link rel="stylesheet" href="/${p}">`).join("");
}

// The entry key in Vite's manifest is the input path the client build
// started from — for this demo that's `index.html` (Vite's default
// entry), which transitively pulls in `src/entry-client.tsx` via the
// inline `<script type="module">` tag in `index.html`.
const ENTRY_SRC = "index.html";

function shell(body: string, props: string): string {
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>ssr-blog — zeroship</title>
    ${clientStylesheet(ENTRY_SRC)}
  </head>
  <body>
    <div id="root">${body}</div>
    <script>window.__SSR_PROPS__ = ${props};</script>
    ${clientScriptTag(ENTRY_SRC)}
  </body>
</html>`;
}

// ── RPC procedures (also usable server-side via .useQuery) ───────────
//
// `listPosts` is a regular RPC procedure marked with `query()`. The
// vite-plugin's transform monkey-patches React Query hooks onto every
// wrapped server-module export, so the App component can call
// `listPosts.useQuery()` BOTH from the browser (hits HTTP
// /_zs/v1/listPosts) AND from the SSR renderer (calls impl directly
// via __makeServerProcedure's hook).

export const listPosts = query(async () => POSTS, { id: "listPosts" });

// ── SSR fetch handler ────────────────────────────────────────────────

import { QueryClient, QueryClientProvider, dehydrate, HydrationBoundary } from "@tanstack/react-query";

export default {
  async fetch(req: Request): Promise<Response> {
    const url = new URL(req.url);
    if (req.method !== "GET") {
      return new Response("Method Not Allowed", { status: 405 });
    }

    // Per-request QueryClient. Prefetch the data we know the page
    // needs, then render — useQuery hooks land on cached data with
    // no loading state.
    const qc = new QueryClient();
    // `listPosts.prefetch` is attached by __makeServerProcedure (see
    // SSR hooks block at top of bundle). Calls impl directly, no HTTP.
    const lp = listPosts as typeof listPosts & {
      prefetch: (input: undefined, qc: QueryClient) => Promise<unknown>;
    };
    await lp.prefetch(undefined, qc);

    const html = renderToString(
      createElement(
        QueryClientProvider,
        { client: qc },
        createElement(HydrationBoundary, { state: dehydrate(qc) },
          createElement(App, { url: url.pathname }),
        ),
      ),
    );
    // Dehydrated state ships in the HTML for client-side rehydration.
    const props = JSON.stringify({
      url: url.pathname,
      dehydrated: dehydrate(qc),
    });
    return new Response(shell(html, props), {
      status: 200,
      headers: { "content-type": "text/html; charset=utf-8" },
    });
  },
};
