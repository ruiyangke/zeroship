// Server entry — what the spec describes.
//
// In a working SSR pipeline, this is the file the build pipeline would
// bundle into `dist/server/index.js` and the V8 worker would invoke
// `default.fetch(request)` on every non-static request.
//
// THE BUILD PIPELINE TODAY DOES NOT USE THIS FILE. The vite-plugin's
// `findServerEntry` picks `src/server.ts` first, which we've shaped to
// expose a "use server" RPC method as a workaround. This file is here
// for documentation — once the build pipeline learns to thread a
// user-defined `default.fetch` through (instead of overwriting it with
// the RPC bootstrap), `vite.config.ts` should switch the `serverEntry`
// to point here and `src/server.ts` can disappear.
//
// See README → "Known limitations" → gap (2).

import { renderToString } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { App } from "./components/App";

// The Vite client manifest baked at build time — Vite emits it at
// `dist/.vite/manifest.json` when `build.manifest: true`. To get it into
// the SSR bundle, the build pipeline would need to either copy it into a
// stable import path (e.g. `dist/server/manifest.json`) before the SSR
// build runs, or expose a virtual module the server bundle can import.
// Today neither path is wired up.
//
// For demo shape we hard-code the entry-client filename; a real
// implementation reads it from the Vite manifest.
const ENTRY_CLIENT = "/_assets/entry-client.js";

const SHELL = `<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>ssr-blog — zeroship</title>
  </head>
  <body>
    <div id="root">{{HTML}}</div>
    <script>window.__SSR_PROPS__ = {{PROPS}};</script>
    <script type="module" src="${ENTRY_CLIENT}"></script>
  </body>
</html>`;

export default {
  async fetch(req: Request): Promise<Response> {
    const url = new URL(req.url);
    if (req.method !== "GET") {
      return new Response("Method Not Allowed", { status: 405 });
    }

    const queryClient = new QueryClient();
    const html = renderToString(
      <QueryClientProvider client={queryClient}>
        <App url={url.pathname} />
      </QueryClientProvider>,
    );
    const props = JSON.stringify({ url: url.pathname });
    const page = SHELL
      .replace("{{HTML}}", html)
      .replace("{{PROPS}}", props);
    return new Response(page, {
      status: 200,
      headers: { "content-type": "text/html; charset=utf-8" },
    });
  },
};
