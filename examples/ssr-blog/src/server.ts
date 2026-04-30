// Server entry — picked up by `@zeroship/vite-plugin` and bundled into
// `dist/server/index.js`. The plugin appends the prelude (always) and
// the dispatchRpc/default.fetch bootstrap (ONLY when this file has no
// own `export default`). Since we DO export default below, the
// bootstrap is skipped and our `default.fetch` is the runtime entry.
//
// See README. Real per-request SSR: every GET hits this fetch handler
// and gets back a freshly-rendered HTML page.

import { renderToString } from "react-dom/server";
import { createElement } from "react";
import { App } from "./components/App";
import { POSTS } from "./components/posts";

// TODO(bug 3): the entry-client filename is hashed at build time. Read
// it from the Vite client manifest via `virtual:zeroship/client-manifest`
// so the rendered HTML loads the right `/_assets/<hash>.js`. For now,
// hard-code the unhashed dev path; production builds will 404 until
// bug 3 lands.
const ENTRY_CLIENT = "/src/entry-client.tsx";

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
    const html = renderToString(createElement(App, { url: url.pathname, posts: POSTS }));
    const props = JSON.stringify({ url: url.pathname });
    const page = SHELL.replace("{{HTML}}", html).replace("{{PROPS}}", props);
    return new Response(page, {
      status: 200,
      headers: { "content-type": "text/html; charset=utf-8" },
    });
  },
};
